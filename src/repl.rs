use anyhow::Result;
use rustyline::error::ReadlineError;
use rustyline::{Config, DefaultEditor};

use crate::cli::Cli;
use crate::eval::Shell;
use crate::prompt::{build_prompt, history_file};

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
    }

    loop {
        let prompt = build_prompt(&shell);

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

                match crate::parser::parse(&full_input) {
                    Err(e) => eprintln!("swagsh: {e}"),
                    Ok(program) => {
                        if !cli.no_execute
                            && let Err(e) = shell.run_program(&program)
                        {
                            eprintln!("swagsh: {e}");
                        }
                    }
                }
                shell.jobs.reap_nonblocking();
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

pub fn source_startup_files(shell: &mut Shell, is_login: bool) {
    let Some(home_str) = shell.env.get("HOME") else {
        return;
    };
    let home = std::path::PathBuf::from(home_str);
    if is_login {
        source_if_exists(shell, &home.join(".swagsh_profile"));
    } else {
        source_if_exists(shell, &home.join(".swagshrc"));
    }
}

fn source_if_exists(shell: &mut Shell, path: &std::path::Path) {
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            eprintln!("swagsh: {}: {e}", path.display());
            return;
        }
    };
    match crate::parser::parse(&src) {
        Ok(program) => {
            let _ = shell.run_program(&program);
        }
        Err(e) => eprintln!("swagsh: {}: {e}", path.display()),
    }
}

fn collect_heredoc_input(line: &str, rl: &mut DefaultEditor) -> String {
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
            // skip <<< (herestring, not heredoc)
            if chars.peek() == Some(&'<') {
                chars.next();
                continue;
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
