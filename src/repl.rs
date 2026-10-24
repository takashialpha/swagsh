use std::io::{IsTerminal, Write};
use std::rc::Rc;

use anyhow::Result;
use rustix::process::{getegid, geteuid, getgid, getuid};
use rustyline::error::ReadlineError;
use rustyline::{Config, DefaultEditor};

use crate::ast::Program;
use crate::builtins::flow::ExitRequest;
use crate::cli::Cli;
use crate::errfmt::emit;
use crate::eval::{Shell, is_interrupted};
use crate::jobs::ExitStatus;
use crate::parser::{Aliases, ParseError};
use crate::prompt::{build_prompt, history_file};
use crate::signal::take_interrupted;

/// # Errors
///
/// Returns an error if the line editor fails to initialize.
pub fn run_interactive(mut shell: Shell, cli: &Cli) -> Result<()> {
    let histsize: usize = shell
        .env
        .get("HISTSIZE")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000)
        .max(1);

    let config = Config::builder()
        .max_history_size(histsize)?
        .history_ignore_space(true)
        .history_ignore_dups(true)?
        .build();

    let mut rl = DefaultEditor::with_config(config)?;

    let history_path = history_file(&shell.env);
    if !cli.private
        && let Some(ref p) = history_path
    {
        let _ = rl.load_history(p);
        shell.history = rl.history().iter().cloned().collect();
    }

    let mut exit_code = None;
    loop {
        let prompt = build_prompt(&shell);

        match rl.readline(&prompt) {
            Ok(line) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }

                take_interrupted();
                let (full_text, program_result) = read_program(line, &mut rl, &shell.env.aliases());
                if shell.verbose {
                    eprintln!("{full_text}");
                }
                // Recorded once the whole (possibly multi-line) command has
                // been read, as one entry holding every line, rather than
                // just `line` alone: adding only the first line here (before
                // `read_program`) is what a plain `for ...; do` loop entered
                // over several `> `-continued lines used to do, silently
                // losing every line after the first from history instead of
                // recalling the whole command on one Up-arrow press.
                if !cli.private && rl.add_history_entry(full_text.trim()).unwrap_or(false) {
                    shell.history.push(full_text.trim().to_owned());
                }

                match program_result {
                    Err(e) => emit(e),
                    Ok(program) => {
                        if !cli.no_execute
                            && let Err(e) = shell.run_program(&program)
                        {
                            if let Some(ExitRequest(code)) = e.downcast_ref() {
                                exit_code = Some(*code);
                                break;
                            } else if is_interrupted(&e) {
                                println!();
                                shell.last_status = ExitStatus(130);
                            } else {
                                emit(e);
                            }
                        }
                    }
                }
                shell.run_pending_traps();
                if std::mem::take(&mut shell.history_changed) {
                    let _ = rl.clear_history();
                    for entry in &shell.history {
                        let _ = rl.add_history_entry(entry.as_str());
                    }
                }
                shell.jobs.reap_nonblocking();
                // `print!`/`echo -n`/`printf` never end in `\n`, so Rust's
                // line-buffered stdout can leave their output sitting
                // unflushed until something *else* happens to write a
                // newline later. Left alone, that stale output surfaces at
                // the wrong time (interleaved with a later prompt or
                // command's output, or worse: appearing in the middle of
                // the next line rustyline reads). Force it out before
                // drawing the next prompt.
                //
                // That alone isn't enough, though: rustyline's next
                // `readline()` call clears the current terminal line
                // before drawing its prompt, on the assumption that line
                // is either empty or holds its own previous prompt. If a
                // builtin left the cursor mid-line (tracked via
                // `Shell::at_line_start`), that clear would erase the
                // output we just flushed before it's ever seen, so bridge
                // onto a fresh line ourselves first.
                if !shell.at_line_start {
                    println!();
                    shell.at_line_start = true;
                }
                let _ = std::io::stdout().flush();
            }
            Err(ReadlineError::Interrupted) => {}
            Err(ReadlineError::Eof) => break,
            Err(e) => {
                emit(format!("readline: {e}"));
                break;
            }
        }
    }

    if !cli.private
        && let Some(ref p) = history_path
        && let Err(e) = rl.save_history(p)
    {
        emit(format!("history: could not save to {}: {e}", p.display()));
    }
    let code = exit_code.unwrap_or(shell.last_status.0);
    shell.exit(code)
}

/// Sources the profile (login), then `$ENV` and the rc file (interactive).
///
/// The shell counts as interactive when stdin and stderr are both terminals,
/// as POSIX defines it. `$ENV` comes after the profile so the profile can set
/// it, and before the rc file so swagsh-specific settings win.
pub fn source_startup_files(shell: &mut Shell, is_login: bool) {
    let home = shell.env.get("HOME").map(std::path::PathBuf::from);
    if is_login && let Some(home) = &home {
        source_if_exists(shell, &home.join(".swagsh_profile"));
    }
    if !(std::io::stdin().is_terminal() && std::io::stderr().is_terminal()) {
        return;
    }
    source_env_file(shell);
    if !is_login && let Some(home) = &home {
        source_if_exists(shell, &home.join(".swagshrc"));
    }
}

/// POSIX: an interactive shell parameter-expands `$ENV` and sources the
/// file it names, unless the real and effective user or group IDs differ.
fn source_env_file(shell: &mut Shell) {
    if getuid() != geteuid() || getgid() != getegid() {
        return;
    }
    let Some(raw) = shell.env.get("ENV").filter(|s| !s.is_empty()) else {
        return;
    };
    match shell.expand_raw_text(&raw) {
        Ok(path) if !path.is_empty() => source_if_exists(shell, std::path::Path::new(&path)),
        Ok(_) => {}
        Err(e) => emit(format!("ENV: {e}")),
    }
}

fn source_if_exists(shell: &mut Shell, path: &std::path::Path) {
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            emit(format!("{}: {e}", path.display()));
            return;
        }
    };
    if let Err(e) = shell.run_source(&src, &format!("{}: ", path.display())) {
        match e.downcast_ref() {
            Some(ExitRequest(code)) => shell.exit(*code),
            None => emit(e),
        }
    }
}

/// Reads `first_line` plus as many further lines as needed for it to parse
/// as a complete `Program`, printing rustyline's `> ` prompt for each one.
/// Generalizes what heredoc collection already did for `<<`/`<<-` bodies
/// specifically: try to parse, and if the only problem is that the parser
/// ran out of input mid-construct (`ParseError::incomplete`, an unclosed
/// `if`/`while`/`for`/`case`/`{`/`(`, or an unterminated quote), read one
/// more line and retry instead of reporting a hard error immediately.
/// Heredoc bodies are still special-cased per line via
/// `collect_heredoc_input`, since their content is raw text that isn't
/// meant to be parsed as shell syntax itself. A trailing `\` is also
/// special-cased via `join_backslash_continuations`: unlike the other
/// three, it's a *lexical* join of two physical lines into one; by the
/// time it reaches the parser as `Token::Eof`-vs-real-token, `readline()`
/// has already stripped the newline it needed to see, so there's no
/// `incomplete` signal to react to, and it has to be resolved before
/// parsing is even attempted.
/// Returns the full raw text read (every physical line, joined by `\n`)
/// alongside the parse result, so the caller can record the *whole* command
/// as a single history entry regardless of how many lines it took.
fn read_program(
    first_line: &str,
    rl: &mut DefaultEditor,
    aliases: &Rc<Aliases>,
) -> (String, Result<Program, ParseError>) {
    let mut buf = collect_heredoc_input(&join_backslash_continuations(first_line, rl), rl);
    loop {
        match crate::parser::parse_with_aliases(&buf, Rc::clone(aliases)) {
            Ok(program) => return (buf, Ok(program)),
            Err(e) if e.incomplete => {
                let Ok(next_line) = rl.readline("> ") else {
                    return (buf, Err(e));
                };
                let next_line = join_backslash_continuations(&next_line, rl);
                if !buf.ends_with('\n') {
                    buf.push('\n');
                }
                buf.push_str(&collect_heredoc_input(&next_line, rl));
            }
            Err(e) => return (buf, Err(e)),
        }
    }
}

/// Joins `line` with however many further lines are needed to resolve every
/// trailing `\` continuation, stripping each such backslash and splicing
/// the next line directly onto it (matching what an unescaped `\<newline>`
/// inside a single buffer already lexes as: nothing). An odd number of
/// trailing backslashes means the last one escapes the newline; an even
/// number means they're literal escaped backslashes and the line really
/// does end there, the same rule the lexer itself would apply if this
/// were one contiguous buffer instead of one `readline()` call per line.
fn join_backslash_continuations(line: &str, rl: &mut DefaultEditor) -> String {
    let mut line = line.to_owned();
    while line.chars().rev().take_while(|&c| c == '\\').count() % 2 == 1 {
        line.pop();
        let Ok(next) = rl.readline("> ") else { break };
        line.push_str(&next);
    }
    line
}

fn collect_heredoc_input(line: &str, rl: &mut DefaultEditor) -> String {
    let delimiters = extract_heredoc_delimiters(line);
    if delimiters.is_empty() {
        return line.to_owned();
    }

    let mut buf = line.to_owned();
    buf.push('\n');

    for (delim, strip_tabs) in delimiters {
        while let Ok(body_line) = rl.readline("> ") {
            buf.push_str(&body_line);
            buf.push('\n');
            let check = body_line.trim_end_matches('\r');
            let check = if strip_tabs {
                check.trim_start_matches('\t')
            } else {
                check
            };
            if check == delim {
                break;
            }
        }
    }
    buf
}

/// Returns each here-doc's delimiter, with quoting removed, and whether it
/// came from `<<-`.
fn extract_heredoc_delimiters(line: &str) -> Vec<(String, bool)> {
    let mut delims = Vec::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '#' {
            break;
        }
        if c == '<' && chars.peek() == Some(&'<') {
            chars.next();
            let strip_tabs = chars.next_if_eq(&'-').is_some();
            // skip <<< (herestring, not heredoc)
            if chars.peek() == Some(&'<') {
                chars.next();
                continue;
            }
            while matches!(chars.peek(), Some(' ' | '\t')) {
                chars.next();
            }
            let mut delim = String::new();
            let mut quote = None;
            while let Some(ch) = chars.next() {
                match (quote, ch) {
                    (None, '\'' | '"') => quote = Some(ch),
                    (Some(q), _) if ch == q => quote = None,
                    (None, '\\') => delim.extend(chars.next()),
                    (None, _)
                        if ch.is_whitespace()
                            || matches!(ch, ';' | '|' | '&' | '<' | '>' | '(' | ')') =>
                    {
                        break;
                    }
                    _ => delim.push(ch),
                }
            }
            if !delim.is_empty() {
                delims.push((delim, strip_tabs));
            }
        }
    }
    delims
}
