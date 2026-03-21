mod ast;
mod cli;
mod env;
mod exec;
mod lexer;
mod parser;

use anyhow::Result;
use clap::Parser as _;

use cli::Cli;
use env::Env;
use exec::Executor;

fn main() -> Result<()> {
    let argv0 = std::env::args().next().unwrap_or_default();
    let cli = Cli::parse();

    let mut env = Env::from_process();

    // Propagate script arguments into positional parameters.
    if !cli.args.is_empty() {
        env.set_positional_args(cli.args.clone());
    }

    let mut exec = Executor::new(env)?;

    // -c "command string"
    if let Some(cmd) = &cli.command {
        let program = parser::parse(cmd)?;
        if !cli.no_execute {
            let status = exec.run_program(&program)?;
            std::process::exit(status.0);
        }
        return Ok(());
    }

    // script file
    if let Some(path) = &cli.script {
        let src = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        let program = parser::parse(&src)?;
        if !cli.no_execute {
            let status = exec.run_program(&program)?;
            std::process::exit(status.0);
        }
        return Ok(());
    }

    // Interactive REPL
    let _is_login = Cli::login_shell(&argv0);
    run_interactive(exec, &cli)?;

    Ok(())
}

fn run_interactive(mut exec: Executor, cli: &Cli) -> Result<()> {
    use rustyline::error::ReadlineError;
    use rustyline::{Config, Editor};

    let config = Config::builder()
        .max_history_size(1000)?
        .history_ignore_space(true)
        .completion_type(rustyline::CompletionType::List)
        .build();

    let mut rl: Editor<(), rustyline::history::FileHistory> = Editor::with_config(config)?;

    // Load history unless private mode is active.
    let history_path = history_file();
    if !cli.private
        && let Some(ref p) = history_path {
            let _ = rl.load_history(p);
        }

    loop {
        let prompt = build_prompt(&exec);

        match rl.readline(&prompt) {
            Ok(line) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }

                if !cli.private {
                    let _ = rl.add_history_entry(line);
                }

                match parser::parse(line) {
                    Err(e) => eprintln!("swagsh: {e}"),
                    Ok(program) => {
                        if !cli.no_execute
                            && let Err(e) = exec.run_program(&program) {
                                eprintln!("swagsh: {e}");
                            }
                    }
                }

                // Reap any background jobs that have finished.
                exec.jobs.reap_nonblocking();
            }

            Err(ReadlineError::Interrupted) => {
                // Ctrl-C — clear the line, do not exit.
                continue;
            }

            Err(ReadlineError::Eof) => {
                // Ctrl-D — clean exit.
                break;
            }

            Err(e) => {
                eprintln!("swagsh: readline: {e}");
                break;
            }
        }
    }

    if !cli.private
        && let Some(ref p) = history_path {
            let _ = rl.save_history(p);
        }

    Ok(())
}

/// Build the prompt string from $PS1, falling back to a sane default.
fn build_prompt(exec: &Executor) -> String {
    let ps1 = exec.env.get("PS1").unwrap_or_default();
    if ps1.is_empty() {
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| "?".into());
        let uid = unsafe { libc::getuid() };
        let suffix = if uid == 0 { "#" } else { "❯" };
        format!("{cwd} {suffix} ")
    } else {
        // Minimal PS1 escape expansion: \w → cwd, \u → user, \$ → # or $.
        expand_ps1(&ps1, exec)
    }
}

fn expand_ps1(ps1: &str, exec: &Executor) -> String {
    let mut out = String::with_capacity(ps1.len());
    let mut chars = ps1.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('w') => {
                    let cwd = std::env::current_dir()
                        .map(|p| {
                            let s = p.to_string_lossy().to_string();
                            // Replace $HOME prefix with ~
                            let home = exec.env.get_or_empty("HOME");
                            if !home.is_empty() && s.starts_with(&home) {
                                s.replacen(&home, "~", 1)
                            } else {
                                s
                            }
                        })
                        .unwrap_or_else(|_| "?".into());
                    out.push_str(&cwd);
                }
                Some('u') => {
                    out.push_str(&exec.env.get_or_empty("USER"));
                }
                Some('h') => {
                    let host = exec.env.get_or_empty("HOSTNAME");
                    let short = host.split('.').next().unwrap_or(&host);
                    out.push_str(short);
                }
                Some('$') => {
                    let uid = unsafe { libc::getuid() };
                    out.push(if uid == 0 { '#' } else { '$' });
                }
                Some('n') => out.push('\n'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Resolve the history file path: `$HISTFILE` → `~/.swagsh_history` fallback.
fn history_file() -> Option<std::path::PathBuf> {
    if let Ok(p) = std::env::var("HISTFILE") {
        return Some(std::path::PathBuf::from(p));
    }
    dirs_path().map(|mut p| {
        p.push(".swagsh_history");
        p
    })
}

fn dirs_path() -> Option<std::path::PathBuf> {
    std::env::var("HOME").ok().map(std::path::PathBuf::from)
}
