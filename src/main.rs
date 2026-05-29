mod ast;
mod cli;
mod env;
mod exec;
mod lexer;
mod parser;

use color_eyre::eyre::{self, Result};
use clap::Parser as _;
use rustix::process::getuid;

use cli::Cli;
use env::Env;
use exec::{Executor, expand_tilde};

fn main() -> Result<()> {
    color_eyre::install()?;
    let argv0 = std::env::args().next().unwrap_or_default();
    let cli = Cli::parse();

    let mut env = Env::from_process();
    if !cli.args.is_empty() {
        env.set_positional_args(cli.args.clone());
    }

    // -c "command string" — non-interactive
    if let Some(cmd) = &cli.command {
        let mut exec = Executor::new(env, false)?;
        let program = parser::parse(cmd)?;
        if !cli.no_execute {
            let status = exec.run_program(&program)?;
            std::process::exit(status.0);
        }
        return Ok(());
    }

    // Script file — non-interactive
    if let Some(path) = &cli.script {
        let mut exec = Executor::new(env, false)?;
        let src = std::fs::read_to_string(path)
            .map_err(|e| eyre::eyre!("{}: {e}", path.display()))?;
        let program = parser::parse(&src)?;
        if !cli.no_execute {
            let status = exec.run_program(&program)?;
            std::process::exit(status.0);
        }
        return Ok(());
    }

    // Interactive REPL
    let is_login = Cli::login_shell(&argv0);
    let mut exec = Executor::new(env, true)?;
    if !cli.no_config {
        source_startup_files(&mut exec, is_login);
    }
    run_interactive(exec, &cli)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tab completer
// ---------------------------------------------------------------------------

use rustyline::completion::{Completer, FilenameCompleter, Pair};
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::Validator;
use rustyline::{Context, Helper};

struct ShellCompleter {
    file: FilenameCompleter,
    builtin_names: Vec<String>,
}

impl ShellCompleter {
    fn new() -> Self {
        let builtin_names = exec::BUILTINS.iter().map(|(n, _)| n.to_string()).collect();
        Self {
            file: FilenameCompleter::new(),
            builtin_names,
        }
    }
}

struct ShellHelper {
    completer: ShellCompleter,
    /// Raw pointer to the executor's Env so we can read aliases without
    /// threading lifetimes through rustyline's `Helper` trait.
    /// SAFETY: the Executor outlives the Editor — both are on the stack in
    /// `run_interactive` and the Editor is dropped first.
    env_ptr: *const Env,
}

impl ShellHelper {
    fn new(exec: &Executor) -> Self {
        Self {
            completer: ShellCompleter::new(),
            env_ptr: &exec.env as *const Env,
        }
    }

    fn env(&self) -> &Env {
        // SAFETY: see field comment above.
        unsafe { &*self.env_ptr }
    }
}

impl Helper for ShellHelper {}
impl Hinter for ShellHelper {
    type Hint = String;
}
impl Highlighter for ShellHelper {}
impl Validator for ShellHelper {}

impl Completer for ShellHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let before_cursor = &line[..pos];
        let word_start = before_cursor
            .rfind(|c: char| c.is_whitespace())
            .map(|i| i + 1)
            .unwrap_or(0);
        let word = &before_cursor[word_start..];
        let is_first_word = !before_cursor[..word_start].contains(|c: char| !c.is_whitespace());

        if is_first_word {
            let mut candidates: Vec<Pair> = Vec::new();

            for name in &self.completer.builtin_names {
                if name.starts_with(word) {
                    candidates.push(Pair {
                        display: name.clone(),
                        replacement: name.clone(),
                    });
                }
            }

            for name in self.env().alias_names() {
                if name.starts_with(word) {
                    candidates.push(Pair {
                        display: name.to_owned(),
                        replacement: name.to_owned(),
                    });
                }
            }

            if let Some(path_var) = self.env().get("PATH") {
                for dir in path_var.split(':') {
                    if let Ok(entries) = std::fs::read_dir(dir) {
                        for entry in entries.filter_map(|e| e.ok()) {
                            let name = entry.file_name().to_string_lossy().to_string();
                            if name.starts_with(word)
                                && let Ok(meta) = entry.metadata()
                            {
                                use std::os::unix::fs::PermissionsExt;
                                if meta.permissions().mode() & 0o111 != 0 {
                                    candidates.push(Pair {
                                        display: name.clone(),
                                        replacement: name,
                                    });
                                }
                            }
                        }
                    }
                }
            }

            candidates.sort_by(|a, b| a.replacement.cmp(&b.replacement));
            candidates.dedup_by(|a, b| a.replacement == b.replacement);

            if !candidates.is_empty() {
                return Ok((word_start, candidates));
            }
        }

        let tilde_expanded = if word.starts_with('~') {
            Some(expand_tilde(word, self.env()))
        } else {
            None
        };

        let effective_word = tilde_expanded.as_deref().unwrap_or(word);
        let effective_line = if tilde_expanded.is_some() {
            format!("{}{}", &before_cursor[..word_start], effective_word)
        } else {
            line.to_owned()
        };

        let (start, pairs) =
            self.completer
                .file
                .complete(&effective_line, effective_line.len(), ctx)?;
        Ok((start.min(word_start), pairs))
    }
}

// ---------------------------------------------------------------------------
// Interactive REPL
// ---------------------------------------------------------------------------

fn collect_heredoc_input(
    line: &str,
    rl: &mut rustyline::Editor<ShellHelper, rustyline::history::FileHistory>,
) -> String {
    let delimiters = extract_heredoc_delimiters(line);
    if delimiters.is_empty() {
        return line.to_owned();
    }

    let mut buf = line.to_owned();
    buf.push('\n');

    for delim in delimiters {
        let delim = delim.trim_matches(|c| c == '\'' || c == '"');

        #[allow(clippy::while_let_loop)]
        loop {
            match rl.readline("> ") {
                Ok(body_line) => {
                    buf.push_str(&body_line);
                    buf.push('\n');
                    if body_line.trim_end_matches('\r') == delim {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    }
    buf
}

fn extract_heredoc_delimiters(line: &str) -> Vec<String> {
    let mut delims = Vec::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '#' {
            break;
        }
        if c == '<' && chars.peek() == Some(&'<') {
            chars.next();
            if chars.peek() == Some(&'-') {
                chars.next();
            }
            while matches!(chars.peek(), Some(' ') | Some('\t')) {
                chars.next();
            }
            let mut delim = String::new();
            for ch in chars.by_ref() {
                if ch.is_whitespace() || matches!(ch, ';' | '|' | '&') {
                    break;
                }
                delim.push(ch);
            }
            if !delim.is_empty() {
                delims.push(delim);
            }
        }
    }
    delims
}

fn source_startup_files(exec: &mut Executor, is_login: bool) {
    let home = match exec.env.get("HOME") {
        Some(h) => std::path::PathBuf::from(h),
        None => return,
    };
    if is_login {
        source_if_exists(exec, &home.join(".swagsh_profile"));
    }
    source_if_exists(exec, &home.join(".swagshrc"));
}

fn source_if_exists(exec: &mut Executor, path: &std::path::Path) {
    let Ok(src) = std::fs::read_to_string(path) else {
        return;
    };
    match parser::parse(&src) {
        Ok(program) => {
            let _ = exec.run_program(&program);
        }
        Err(e) => eprintln!("swagsh: {}: {e}", path.display()),
    }
}

fn run_interactive(mut exec: Executor, cli: &Cli) -> Result<()> {
    use rustyline::error::ReadlineError;
    use rustyline::{Config, Editor};

    let config = Config::builder()
        .max_history_size(1000)?
        .history_ignore_space(true)
        .completion_type(rustyline::CompletionType::List)
        .build();

    let helper = ShellHelper::new(&exec);
    let mut rl: Editor<ShellHelper, rustyline::history::FileHistory> = Editor::with_config(config)?;
    rl.set_helper(Some(helper));

    let history_path = history_file();
    if !cli.private
        && let Some(ref p) = history_path
    {
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

                let full_input = collect_heredoc_input(line, &mut rl);

                match parser::parse(&full_input) {
                    Err(e) => eprintln!("swagsh: {e}"),
                    Ok(program) => {
                        if !cli.no_execute
                            && let Err(e) = exec.run_program(&program)
                        {
                            eprintln!("swagsh: {e}");
                        }
                    }
                }
                exec.jobs.reap_nonblocking();
            }
            Err(ReadlineError::Interrupted) => continue,
            Err(ReadlineError::Eof) => break,
            Err(e) => {
                eprintln!("swagsh: readline: {e}");
                break;
            }
        }
    }

    if !cli.private
        && let Some(ref p) = history_path
    {
        let _ = rl.save_history(p);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Prompt
// ---------------------------------------------------------------------------

fn build_prompt(exec: &Executor) -> String {
    let ps1 = exec.env.get("PS1").unwrap_or_default();
    if ps1.is_empty() {
        let cwd = current_dir_display(&exec.env);
        let suffix = if getuid().is_root() { "#" } else { "❯" };
        format!("{cwd} {suffix} ")
    } else {
        expand_ps1(&ps1, exec)
    }
}

fn expand_ps1(ps1: &str, exec: &Executor) -> String {
    let mut out = String::with_capacity(ps1.len());
    let mut chars = ps1.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('w') => out.push_str(&current_dir_display(&exec.env)),
            Some('u') => out.push_str(&exec.env.get_or_empty("USER")),
            Some('h') => {
                let host = exec.env.get_or_empty("HOSTNAME");
                out.push_str(host.split('.').next().unwrap_or(&host));
            }
            Some('$') => out.push(if getuid().is_root() { '#' } else { '$' }),
            Some('n') => out.push('\n'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Returns the current working directory with `$HOME` collapsed to `~`.
fn current_dir_display(env: &Env) -> String {
    std::env::current_dir()
        .map(|p| {
            let s = p.to_string_lossy().to_string();
            let home = env.get_or_empty("HOME");
            if !home.is_empty() && s.starts_with(&home) {
                s.replacen(&home, "~", 1)
            } else {
                s
            }
        })
        .unwrap_or_else(|_| "?".into())
}

fn history_file() -> Option<std::path::PathBuf> {
    if let Ok(p) = std::env::var("HISTFILE") {
        return Some(std::path::PathBuf::from(p));
    }
    std::env::var("HOME").ok().map(|h| {
        let mut p = std::path::PathBuf::from(h);
        p.push(".swagsh_history");
        p
    })
}
