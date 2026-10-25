use rustix::process::getuid;

use crate::env::Env;
use crate::eval::Shell;
use crate::expand::expand_tilde;
use crate::sys::strftime_now;

// \x01/\x02 wrap ANSI codes so rustyline measures visible width correctly.
const CYAN: &str = "\x01\x1b[1;36m\x02";
const GREEN: &str = "\x01\x1b[1;32m\x02";
const RED: &str = "\x01\x1b[1;31m\x02";
const RESET: &str = "\x01\x1b[0m\x02";

/// The primary prompt: `$PS1` expanded, or swagsh's own when it is unset or
/// empty.
pub fn build_prompt(shell: &mut Shell) -> String {
    let ps1 = shell.env.get("PS1").unwrap_or_default();
    if !ps1.is_empty() {
        return expand_prompt(shell, &ps1);
    }
    let cwd = current_dir_display(&shell.env);
    if getuid().is_root() {
        format!("{RED}{cwd}{RESET} {RED}#{RESET} ")
    } else {
        let arrow = if shell.last_status.is_success() {
            GREEN
        } else {
            RED
        };
        format!("{CYAN}{cwd}{RESET} {arrow}❯{RESET} ")
    }
}

/// The continuation prompt: `$PS2` expanded, `> ` by default.
pub fn continuation_prompt(shell: &mut Shell) -> String {
    let ps2 = shell.env.get("PS2").unwrap_or_else(|| "> ".to_owned());
    expand_prompt(shell, &ps2)
}

/// Expands prompt string `text` as bash does: its backslash escapes
/// (`\u`, `\w`, `\t`, ...), then parameter expansion, command
/// substitution and arithmetic expansion of the result.
pub fn expand_prompt(shell: &mut Shell, text: &str) -> String {
    let escaped = prompt_escapes(text, shell);
    if !escaped.contains(['$', '`']) {
        return escaped;
    }
    shell.expand_raw_text(&escaped).unwrap_or(escaped)
}

fn prompt_escapes(text: &str, shell: &Shell) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('w') => out.push_str(&current_dir_display(&shell.env)),
            Some('W') => {
                let cwd = current_dir_display(&shell.env);
                let base = cwd.rsplit('/').next().unwrap_or(&cwd);
                out.push_str(if base.is_empty() { "/" } else { base });
            }
            Some('u') => out.push_str(&shell.env.get_or_empty("USER")),
            Some('h') => out.push_str(&short_hostname(&shell.env)),
            Some('H') => out.push_str(&hostname(&shell.env)),
            Some('$') => out.push(if getuid().is_root() { '#' } else { '$' }),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('a') => out.push('\x07'),
            Some('e') => out.push('\x1b'),
            Some('d') => out.push_str(&strftime_now("%a %b %d")),
            Some('t') => out.push_str(&strftime_now("%H:%M:%S")),
            Some('T') => out.push_str(&strftime_now("%I:%M:%S")),
            Some('@') => out.push_str(&strftime_now("%I:%M %p")),
            Some('A') => out.push_str(&strftime_now("%H:%M")),
            Some('D') if chars.peek() == Some(&'{') => {
                chars.next();
                let format: String = chars.by_ref().take_while(|&c| c != '}').collect();
                out.push_str(&strftime_now(if format.is_empty() {
                    "%X"
                } else {
                    &format
                }));
            }
            Some('j') => out.push_str(&shell.jobs.iter().count().to_string()),
            Some('l') => out.push_str(&tty_name()),
            Some('s') => out.push_str("swagsh"),
            Some('v') => {
                let version = env!("CARGO_PKG_VERSION");
                let short: Vec<&str> = version.split('.').take(2).collect();
                out.push_str(&short.join("."));
            }
            Some('V') => out.push_str(env!("CARGO_PKG_VERSION")),
            Some('!') => out.push_str(&(shell.history.len() + 1).to_string()),
            Some('#') => out.push_str(&shell.command_number.to_string()),
            // \[ and \] mark invisible sequences (ANSI colors) so readline can
            // measure the visible prompt width correctly.
            Some('[') => out.push('\x01'),
            Some(']') => out.push('\x02'),
            Some(d @ '0'..='7') => {
                let mut value = d.to_digit(8).unwrap_or(0);
                for _ in 0..2 {
                    match chars.peek().and_then(|c| c.to_digit(8)) {
                        Some(n) => {
                            value = value * 8 + n;
                            chars.next();
                        }
                        None => break,
                    }
                }
                out.extend(char::from_u32(value));
            }
            Some('\\') | None => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
        }
    }
    out
}

/// The terminal standard input is, without its `/dev/`, for `\l`.
fn tty_name() -> String {
    rustix::termios::ttyname(std::io::stdin(), Vec::new()).map_or_else(
        |_| "tty".to_owned(),
        |name| {
            let name = name.to_string_lossy().into_owned();
            name.rsplit('/').next().unwrap_or(&name).to_owned()
        },
    )
}

fn hostname(env: &Env) -> String {
    env.get("HOSTNAME")
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "localhost".to_owned())
}

fn short_hostname(env: &Env) -> String {
    let host = env
        .get("HOSTNAME")
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "localhost".to_owned());
    host.split('.').next().unwrap_or(&host).to_owned()
}

pub fn current_dir_display(env: &Env) -> String {
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
    if s == home {
        "~".to_owned()
    } else if s.starts_with(&format!("{home}/")) {
        format!("~/{}", &s[home.len() + 1..])
    } else {
        s
    }
}

#[must_use]
pub fn history_file(env: &Env) -> Option<std::path::PathBuf> {
    if let Some(raw) = env.get("HISTFILE") {
        if raw.is_empty() {
            return None;
        }
        return Some(std::path::PathBuf::from(
            expand_tilde(&raw, env).into_owned(),
        ));
    }
    env.get("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".swagsh_history"))
}
