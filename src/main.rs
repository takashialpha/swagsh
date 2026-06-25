mod ast;
mod cli;
mod env;
mod exec;
mod lexer;
mod parser;

use anyhow::{Result, anyhow};
use clap::Parser as _;
use rustix::process::getuid;

use cli::Cli;
use env::Env;
use exec::{Executor, expand_tilde};

fn main() -> Result<()> {
    let argv0 = std::env::args().next().unwrap_or_default();
    let cli = Cli::parse();

    let (script, args) = cli.split_positionals();

    let mut env = Env::from_process();
    if !args.is_empty() {
        env.set_positional_args(args);
    }

    // Seed $HOSTNAME once so PS1 \h doesn't hit /etc/hostname on every prompt.
    if env.get("HOSTNAME").is_none_or(|h| h.is_empty())
        && let Ok(name) = std::fs::read_to_string("/etc/hostname")
    {
        let name = name.trim().to_owned();
        if !name.is_empty() {
            env.set("HOSTNAME", name);
        }
    }

    // -c "command string": non-interactive
    if let Some(cmd) = &cli.command {
        let mut exec = Executor::new(env, false);
        let program = parser::parse(cmd)?;
        if !cli.no_execute {
            let status = exec.run_program(&program)?;
            std::process::exit(status.0);
        }
        return Ok(());
    }

    // Script file: non-interactive
    if let Some(path) = &script {
        let mut exec = Executor::new(env, false);
        let src = std::fs::read_to_string(path).map_err(|e| anyhow!("{}: {e}", path.display()))?;
        let program = parser::parse(&src)?;
        if !cli.no_execute {
            let status = exec.run_program(&program)?;
            std::process::exit(status.0);
        }
        return Ok(());
    }

    // Interactive REPL
    let is_login = cli.login || Cli::is_login_shell(&argv0);
    let mut exec = Executor::new(env, true);
    if !cli.no_rc {
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
    /// Raw pointer to `Executor::env`. Needed because rustyline's `Helper`
    /// trait has no lifetime parameter, so we cannot store a `&Env` directly.
    ///
    /// SAFETY invariants (all hold in `run_interactive`):
    ///   1. `exec` is never moved after this pointer is taken (`&raw const`).
    ///   2. The `Editor` (which holds this helper) is a local declared after
    ///      `exec`, so it is dropped before `exec` (Rust drops locals in
    ///      reverse order; parameters outlive locals).
    ///   3. This pointer is only dereferenced inside `readline`, which blocks
    ///      for user input. `exec` is not mutated while `readline` is running.
    env_ptr: *const Env,
}

impl ShellHelper {
    fn new(exec: &Executor) -> Self {
        Self {
            completer: ShellCompleter::new(),
            env_ptr: &raw const exec.env,
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
            .map_or(0, |i| i + 1);
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
                        for entry in entries.filter_map(std::result::Result::ok) {
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

        while let Ok(body_line) = rl.readline("> ") {
            buf.push_str(&body_line);
            buf.push('\n');
            if body_line.trim_end_matches('\r') == delim {
                break;
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
            while matches!(chars.peek(), Some(' ' | '\t')) {
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
    let Some(home_str) = exec.env.get("HOME") else {
        return;
    };
    let home = std::path::PathBuf::from(home_str);
    // Login shells source the profile only. Non-login interactive shells source
    // the rc only. Users who want both source the rc from their profile.
    if is_login {
        source_if_exists(exec, &home.join(".swagsh_profile"));
    } else {
        source_if_exists(exec, &home.join(".swagshrc"));
    }
}

fn source_if_exists(exec: &mut Executor, path: &std::path::Path) {
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            eprintln!("swagsh: {}: {e}", path.display());
            return;
        }
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

    let histsize: usize = exec
        .env
        .get("HISTSIZE")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000)
        .max(1);

    let config = Config::builder()
        .max_history_size(histsize)?
        .history_ignore_space(true)
        .history_ignore_dups(true)?
        .completion_type(rustyline::CompletionType::List)
        .build();

    let helper = ShellHelper::new(&exec);
    let mut rl: Editor<ShellHelper, rustyline::history::FileHistory> = Editor::with_config(config)?;
    rl.set_helper(Some(helper));

    let history_path = history_file(&exec.env);
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
            Err(ReadlineError::Interrupted) => {}
            Err(ReadlineError::Eof) => break,
            Err(e) => {
                eprintln!("swagsh: readline: {e}");
                break;
            }
        }
    }

    if !cli.private
        && let Some(ref p) = history_path
        && let Err(e) = rl.save_history(p)
    {
        eprintln!("swagsh: history: could not save to {}: {e}", p.display());
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
    let mut out = String::with_capacity(ps1.len() + 16);
    let mut chars = ps1.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('w') => out.push_str(&current_dir_display(&exec.env)),
            Some('W') => {
                let cwd = current_dir_display(&exec.env);
                let base = cwd.rsplit('/').next().unwrap_or(&cwd);
                out.push_str(if base.is_empty() { "/" } else { base });
            }
            Some('u') => out.push_str(&exec.env.get_or_empty("USER")),
            Some('h') => out.push_str(&short_hostname(&exec.env)),
            Some('$') => out.push(if getuid().is_root() { '#' } else { '$' }),
            Some('n') => out.push('\n'),
            Some('e') => out.push('\x1b'),
            // \[ and \] mark invisible sequences (ANSI colors) so readline can
            // measure the visible prompt width correctly.
            Some('[') => out.push('\x01'),
            Some(']') => out.push('\x02'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn short_hostname(env: &Env) -> String {
    // $HOSTNAME is seeded from /etc/hostname at startup if not already set.
    let host = env
        .get("HOSTNAME")
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "localhost".to_owned());
    host.split('.').next().unwrap_or(&host).to_owned()
}

/// Returns the current working directory with `$HOME` collapsed to `~`.
/// Prefers `$PWD` (logical path, preserves symlinks) over the kernel cwd.
fn current_dir_display(env: &Env) -> String {
    let cwd = env
        .get("PWD")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| std::path::PathBuf::from("?"));

    let s = cwd.to_string_lossy().into_owned();
    let home = env.get_or_empty("HOME");

    if home.is_empty() {
        return s;
    }
    // Check at a path boundary to avoid /home/foobar collapsing to ~/bar.
    if s == home {
        "~".to_owned()
    } else if s.starts_with(&format!("{home}/")) {
        format!("~/{}", &s[home.len() + 1..])
    } else {
        s
    }
}

fn history_file(env: &Env) -> Option<std::path::PathBuf> {
    // $HISTFILE="" explicitly disables history; otherwise expand tilde and use it.
    if let Some(raw) = env.get("HISTFILE") {
        if raw.is_empty() {
            return None;
        }
        return Some(std::path::PathBuf::from(expand_tilde(&raw, env)));
    }
    // Default: ~/.swagsh_history
    env.get("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".swagsh_history"))
}
