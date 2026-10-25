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
use crate::prompt::{build_prompt, continuation_prompt, history_file};
use crate::signal::take_interrupted;

mod histexpand;
mod notify;

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
        announce_jobs(&mut shell);
        let input = read_line(&mut shell, &mut rl);
        match input {
            Ok(line) => {
                let line = line.trim().to_owned();
                if line.is_empty() {
                    continue;
                }
                let Some(line) = expand_history(&mut shell, &mut rl, line, cli.private) else {
                    continue;
                };
                let line = line.as_str();

                take_interrupted();
                shell.command_number += 1;
                let ps2 = continuation_prompt(&mut shell);
                let (full_text, program_result) =
                    read_program(line, &mut rl, &shell.env.aliases(), &ps2);
                if shell.verbose {
                    eprint!("{full_text}");
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

    let save = |rl: &mut DefaultEditor, p: &std::path::Path| {
        if shell.shopt.histappend {
            rl.append_history(p)
        } else {
            rl.save_history(p)
        }
    };
    if !cli.private
        && let Some(ref p) = history_path
        && let Err(e) = save(&mut rl, p)
    {
        emit(format!("history: could not save to {}: {e}", p.display()));
    }
    let code = exit_code.unwrap_or(shell.last_status.0);
    shell.exit(code)
}

/// Reports the jobs that stopped or finished, before a prompt.
fn announce_jobs(shell: &mut Shell) {
    shell.jobs.reap_nonblocking();
    if shell.monitor {
        for line in shell.jobs.take_stopped() {
            eprintln!("{line}");
        }
        for line in shell.jobs.take_finished() {
            eprintln!("{line}");
        }
    }
}

/// Shows the prompt and reads a line, announcing jobs meanwhile under
/// `set -b`.
fn read_line(shell: &mut Shell, rl: &mut DefaultEditor) -> rustyline::Result<String> {
    let prompt = build_prompt(shell);
    let watcher = notify::Watcher::start(shell, rl);
    let input = rl.readline(&prompt);
    if let Some(watcher) = watcher {
        let announced = watcher.finish();
        for id in announced.done {
            shell.jobs.mark_notified(id);
        }
        for id in announced.stopped {
            shell.jobs.mark_stop_announced(id);
        }
    }
    input
}

/// `line` after history expansion (`set -H`), shown when it changed.
/// `None` when there is nothing to run: an expansion failed, or `:p` only
/// asked to see it.
fn expand_history(
    shell: &mut Shell,
    rl: &mut DefaultEditor,
    line: String,
    private: bool,
) -> Option<String> {
    if !shell.histexpand {
        return Some(line);
    }
    match histexpand::expand(&line, &shell.history) {
        Ok(None) => Some(line),
        Ok(Some(expanded)) => {
            println!("{}", expanded.line);
            if expanded.print_only {
                if !private && rl.add_history_entry(&expanded.line).unwrap_or(false) {
                    shell.history.push(expanded.line);
                }
                return None;
            }
            Some(expanded.line)
        }
        Err(msg) => {
            emit(msg);
            None
        }
    }
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
/// as a complete `Program`, printing the continuation prompt (`$PS2`) for each one.
///
/// Whenever the only problem is that the input ran out mid-construct
/// (`ParseError::incomplete`: an unclosed `if`/`while`/`for`/`case`/`{`/`(`,
/// an unterminated quote, or a here-document still waiting for its
/// delimiter, whether its `<<` was typed or came from an alias), one more
/// line is read and the whole text parsed again. A line inside a
/// here-document is taken as it stands; any other line first has its
/// trailing `\` continuations joined (see `join_backslash_continuations`).
///
/// Returns the full text read, every physical line joined by `\n`, so the
/// caller can record the whole command as a single history entry.
fn read_program(
    first_line: &str,
    rl: &mut DefaultEditor,
    aliases: &Rc<Aliases>,
    ps2: &str,
) -> (String, Result<Program, ParseError>) {
    let mut buf = join_backslash_continuations(first_line, rl, ps2);
    buf.push('\n');
    loop {
        match crate::parser::parse_with_aliases(&buf, Rc::clone(aliases)) {
            Ok(program) => return (buf, Ok(program)),
            Err(e) if e.incomplete => {
                let Ok(next_line) = rl.readline(ps2) else {
                    return (buf, Err(e));
                };
                if e.in_heredoc {
                    buf.push_str(&next_line);
                } else {
                    buf.push_str(&join_backslash_continuations(&next_line, rl, ps2));
                }
                buf.push('\n');
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
fn join_backslash_continuations(line: &str, rl: &mut DefaultEditor, ps2: &str) -> String {
    let mut line = line.to_owned();
    while line.chars().rev().take_while(|&c| c == '\\').count() % 2 == 1 {
        line.pop();
        let Ok(next) = rl.readline(ps2) else { break };
        line.push_str(&next);
    }
    line
}
