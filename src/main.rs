mod ast;
mod builtins;
mod cli;
mod env;
mod errfmt;
mod eval;
mod expand;
mod fd;
mod jobs;
mod lexer;
mod parser;
mod prompt;
mod repl;
mod signal;

use anyhow::{Result, anyhow};
use clap::Parser as _;

use cli::Cli;
use env::Env;
use errfmt::{emit, strerror};
use eval::Shell;

fn main() -> Result<()> {
    signal::reset_sigpipe();
    let argv0 = std::env::args().next().unwrap_or_default();
    let cli = Cli::parse();

    let (script, args) = cli.split_positionals();

    let mut env = Env::from_process();
    if !args.is_empty() {
        env.set_positional_args(args);
    }

    if env.get("HOSTNAME").is_none_or(|h| h.is_empty())
        && let Ok(name) = std::fs::read_to_string("/etc/hostname")
    {
        let name = name.trim().to_owned();
        if !name.is_empty() {
            env.set("HOSTNAME", name);
        }
    }

    if let Some(cmd) = &cli.command {
        let mut shell = Shell::new(env, false);
        return run_and_exit(&mut shell, cmd, &cli);
    }

    if let Some(path) = &script {
        let mut shell = Shell::new(env, false);
        let src = std::fs::read_to_string(path)
            .map_err(|e| anyhow!("{}: {}", path.display(), strerror(e)))?;
        return run_and_exit(&mut shell, &src, &cli);
    }

    let is_login = cli.login || Cli::is_login_shell(&argv0);
    let mut shell = Shell::new(env, true);
    if !cli.no_rc {
        repl::source_startup_files(&mut shell, is_login);
    }
    repl::run_interactive(shell, &cli)?;
    Ok(())
}

/// Parses and (unless `--dry-run`) runs `src`, exiting with the program's
/// status. Parse and execution errors go through `errfmt::emit`, the same
/// path the interactive REPL uses, instead of being left to propagate via
/// `?` to `main`'s default `Result` handler: that would print them as
/// `Error: <Debug>` (a different prefix than the rest of swagsh's output,
/// and without `emit`'s `(os error N)` stripping). Errors that don't fork
/// (a builtin's or a shell function's own errors, since `run_builtin` and
/// `run_function` run in this same process) take exactly this path, so
/// skipping it here left them unformatted for `-c` and script invocations.
fn run_and_exit(shell: &mut Shell, src: &str, cli: &Cli) -> Result<()> {
    let program = match parser::parse(src) {
        Ok(p) => p,
        Err(e) => {
            emit(e);
            std::process::exit(1);
        }
    };
    if cli.no_execute {
        return Ok(());
    }
    match shell.run_program(&program) {
        Ok(status) => std::process::exit(status.0),
        Err(e) => {
            emit(e);
            std::process::exit(1);
        }
    }
}
