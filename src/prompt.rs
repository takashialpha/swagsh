use rustix::process::getuid;

use crate::env::Env;
use crate::eval::Shell;
use crate::expand::expand_tilde;

pub fn build_prompt(shell: &Shell) -> String {
    let ps1 = shell.env.get("PS1").unwrap_or_default();
    if ps1.is_empty() {
        let cwd = current_dir_display(&shell.env);
        let suffix = if getuid().is_root() { "#" } else { "❯" };
        format!("{cwd} {suffix} ")
    } else {
        expand_ps1(&ps1, shell)
    }
}

fn expand_ps1(ps1: &str, shell: &Shell) -> String {
    let mut out = String::with_capacity(ps1.len() + 16);
    let mut chars = ps1.chars();
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

pub fn history_file(env: &Env) -> Option<std::path::PathBuf> {
    if let Some(raw) = env.get("HISTFILE") {
        if raw.is_empty() {
            return None;
        }
        return Some(std::path::PathBuf::from(expand_tilde(&raw, env)));
    }
    env.get("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".swagsh_history"))
}
