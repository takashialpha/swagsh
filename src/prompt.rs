use std::borrow::Cow;

use rustix::process::getuid;

use crate::env::Env;
use crate::eval::Shell;
use crate::expand::expand_tilde;
use crate::repl::config::{self, ansi};
use crate::repl::git;
use crate::sys::strftime_now;

/// Style `spec` (see [`ansi`]) wrapped in `\x01`/`\x02`, so the line
/// editor knows it takes no room.
fn style(spec: &str) -> String {
    match ansi(spec) {
        Ok(code) if !code.is_empty() => format!("\x01{code}\x02"),
        _ => String::new(),
    }
}

const RESET: &str = "\x01\x1b[0m\x02";

/// The parts of the prompt only swagsh has, each with a space before it
/// and empty when there is nothing to show.
#[derive(Default)]
struct Extras {
    /// `\g`: the git branch and its state.
    git: Option<String>,
    /// `\c`: how long the last command took.
    took: Option<String>,
}

/// The primary prompt: `$PS1` when it is set, else `prompt.format`, else
/// swagsh's own: the directory, the git branch, how long the last command
/// took, and a symbol colored by whether it worked. `took` is how long the
/// last command ran, if one did.
pub fn build_prompt(
    shell: &mut Shell,
    settings: &config::Prompt,
    took: Option<std::time::Duration>,
) -> String {
    let ps1 = shell.env.get("PS1").unwrap_or_default();
    let text = if ps1.is_empty() {
        settings.format.clone()
    } else {
        ps1
    };
    if !text.is_empty() {
        let extras = Extras {
            git: text
                .contains("\\g")
                .then(|| git_part(shell, &settings.git, "")),
            took: text
                .contains("\\c")
                .then(|| took_part(took, &settings.duration, "")),
        };
        return expand(shell, &text, &extras);
    }
    let cwd = current_dir_display(&shell.env);
    let git = git_part(shell, &settings.git, "on ");
    let took = took_part(took, &settings.duration, "took ");
    let between = if settings.newline { "\n" } else { " " };
    if getuid().is_root() {
        let red = style(&settings.failure);
        format!("{red}{cwd}{RESET}{git}{took}{between}{red}#{RESET} ")
    } else {
        let symbol = style(if shell.last_status.is_success() {
            &settings.success
        } else {
            &settings.failure
        });
        let directory = style(&settings.directory);
        format!(
            "{directory}{cwd}{RESET}{git}{took}{between}{symbol}{}{RESET} ",
            settings.symbol
        )
    }
}

/// `text` in style `spec`, with a space and `label` before it.
fn part(label: &str, text: &str, spec: &str) -> String {
    let color = style(spec);
    let reset = if color.is_empty() { "" } else { RESET };
    format!(" {label}{color}{text}{reset}")
}

/// The git branch and its state, after `label`; empty outside a
/// repository.
fn git_part(shell: &Shell, settings: &config::Git, label: &str) -> String {
    if !settings.enabled {
        return String::new();
    }
    let Ok(dir) = std::env::current_dir() else {
        return String::new();
    };
    git::info(&dir, settings.status, &shell.env).map_or_else(String::new, |info| {
        part(label, &info.render(), &settings.color)
    })
}

/// How long the last command took, after `label`, if that was long
/// enough to show.
fn took_part(
    took: Option<std::time::Duration>,
    settings: &config::Duration,
    label: &str,
) -> String {
    match took {
        Some(took) if settings.enabled && took.as_secs_f64() >= settings.min => {
            part(label, &format_duration(took), &settings.color)
        }
        _ => String::new(),
    }
}

/// `took` as the prompt shows it: `850ms`, `2.3s`, `42s`, `1m5s`, `2h3m`.
fn format_duration(took: std::time::Duration) -> String {
    let secs = took.as_secs();
    match secs {
        0 => format!("{}ms", took.as_millis()),
        1..10 => format!("{:.1}s", took.as_secs_f64()),
        10..60 => format!("{secs}s"),
        60..3600 => format!("{}m{}s", secs / 60, secs % 60),
        _ => format!("{}h{}m", secs / 3600, secs % 3600 / 60),
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
    expand(shell, text, &Extras::default())
}

/// [`expand_prompt`], with the escapes only swagsh has.
fn expand(shell: &mut Shell, text: &str, extras: &Extras) -> String {
    let expands = text.contains(['$', '`']);
    let escaped = prompt_escapes(text, shell, expands, extras);
    if !expands {
        return escaped;
    }
    shell.expand_raw_text(&escaped).unwrap_or(escaped)
}

fn prompt_escapes(text: &str, shell: &Shell, quote: bool, extras: &Extras) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let value: Cow<'_, str> = match chars.next() {
            Some('w') => current_dir_display(&shell.env).into(),
            Some('W') => {
                let cwd = current_dir_display(&shell.env);
                let base = cwd.rsplit('/').next().unwrap_or(&cwd);
                (if base.is_empty() { "/" } else { base }).to_owned().into()
            }
            Some('u') => shell.env.get_or_empty("USER").into(),
            Some('h') => short_hostname(&shell.env).into(),
            Some('H') => hostname(&shell.env).into(),
            Some('$') => (if getuid().is_root() { "#" } else { "$" }).into(),
            Some('n') => "\n".into(),
            Some('r') => "\r".into(),
            Some('a') => "\x07".into(),
            Some('e') => "\x1b".into(),
            Some('d') => strftime_now("%a %b %d").into(),
            Some('t') => strftime_now("%H:%M:%S").into(),
            Some('T') => strftime_now("%I:%M:%S").into(),
            Some('@') => strftime_now("%I:%M %p").into(),
            Some('A') => strftime_now("%H:%M").into(),
            Some('D') if chars.peek() == Some(&'{') => {
                chars.next();
                let format: String = chars.by_ref().take_while(|&c| c != '}').collect();
                strftime_now(if format.is_empty() { "%X" } else { &format }).into()
            }
            Some('j') => shell.jobs.iter().count().to_string().into(),
            Some('l') => tty_name().into(),
            Some('s') => "swagsh".into(),
            Some('v') => {
                let version = env!("CARGO_PKG_VERSION");
                let short: Vec<&str> = version.split('.').take(2).collect();
                short.join(".").into()
            }
            Some('V') => env!("CARGO_PKG_VERSION").into(),
            Some('!') => (shell.history.len() + 1).to_string().into(),
            Some('#') => shell.command_number.to_string().into(),
            Some('g') if extras.git.is_some() => extras.git.as_deref().unwrap_or_default().into(),
            Some('c') if extras.took.is_some() => extras.took.as_deref().unwrap_or_default().into(),
            // \[ and \] mark invisible sequences (ANSI colors) so readline can
            // measure the visible prompt width correctly.
            Some('[') => "\x01".into(),
            Some(']') => "\x02".into(),
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
                char::from_u32(value)
                    .map(String::from)
                    .unwrap_or_default()
                    .into()
            }
            Some('\\') | None => "\\".into(),
            Some(other) => format!("\\{other}").into(),
        };
        // What an escape stands for is text, never expanded itself: a
        // directory named `$(cmd)` must not run cmd.
        if quote {
            for c in value.chars() {
                if matches!(c, '$' | '`' | '\\') {
                    out.push('\\');
                }
                out.push(c);
            }
        } else {
            out.push_str(&value);
        }
    }
    out
}

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
