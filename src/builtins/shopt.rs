//! `shopt`: the shell options beyond `set`'s.

use anyhow::Result;

use super::BuiltinFailed;
use crate::errfmt::emit;
use crate::eval::Shell;
use crate::jobs::ExitStatus;

/// Every option `shopt` knows, in the order it lists them.
const NAMES: &[&str] = &[
    "autocd",
    "dotglob",
    "expand_aliases",
    "extglob",
    "failglob",
    "globstar",
    "histappend",
    "inherit_errexit",
    "lastpipe",
    "login_shell",
    "nocaseglob",
    "nocasematch",
    "nullglob",
    "sourcepath",
    "xpg_echo",
];

/// A `bool` of the shell's that an option name stands for.
fn option<'a>(shell: &'a mut Shell, name: &str) -> Option<&'a mut bool> {
    let o = &mut shell.shopt;
    Some(match name {
        "autocd" => &mut o.autocd,
        "dotglob" => &mut o.dotglob,
        "failglob" => &mut o.failglob,
        "globstar" => &mut o.globstar,
        "histappend" => &mut o.histappend,
        "inherit_errexit" => &mut o.inherit_errexit,
        "lastpipe" => &mut o.lastpipe,
        "nocaseglob" => &mut o.nocaseglob,
        "nocasematch" => &mut o.nocasematch,
        "nullglob" => &mut o.nullglob,
        "sourcepath" => &mut o.sourcepath,
        "xpg_echo" => &mut o.xpg_echo,
        "expand_aliases" => &mut shell.env.expand_aliases,
        _ => return None,
    })
}

/// Whether option `name` is on; `None` for an unknown one.
fn value(shell: &Shell, name: &str) -> Option<bool> {
    let o = &shell.shopt;
    Some(match name {
        "autocd" => o.autocd,
        "dotglob" => o.dotglob,
        "expand_aliases" => shell.env.expand_aliases,
        "extglob" => crate::pattern::enabled(),
        "failglob" => o.failglob,
        "globstar" => o.globstar,
        "histappend" => o.histappend,
        "inherit_errexit" => o.inherit_errexit,
        "lastpipe" => o.lastpipe,
        "login_shell" => shell.login,
        "nocaseglob" => o.nocaseglob,
        "nocasematch" => o.nocasematch,
        "nullglob" => o.nullglob,
        "sourcepath" => o.sourcepath,
        "xpg_echo" => o.xpg_echo,
        _ => return None,
    })
}

/// The names of the options that are on, for `BASHOPTS`.
#[must_use]
pub fn on(shell: &Shell) -> Vec<&'static str> {
    NAMES
        .iter()
        .copied()
        .filter(|name| value(shell, name) == Some(true))
        .collect()
}

/// Turns option `name` on or off. False for an unknown or read-only one.
fn set(shell: &mut Shell, name: &str, on: bool) -> bool {
    if name == "extglob" {
        crate::pattern::set_enabled(on);
        return true;
    }
    option(shell, name).is_some_and(|o| {
        *o = on;
        true
    })
}

/// `shopt [-pqsu] [-o] [optname ...]`.
///
/// # Errors
///
/// Returns an error for a bad option or a failed write.
pub fn builtin_shopt(shell: &mut Shell, args: &[&str]) -> Result<ExitStatus> {
    let (mut on, mut off, mut quiet, mut print, mut set_o) = (false, false, false, false, false);
    let mut names = args;
    while let Some((&first, rest)) = names.split_first() {
        if first == "--" {
            names = rest;
            break;
        }
        let Some(flags) = first.strip_prefix('-').filter(|f| !f.is_empty()) else {
            break;
        };
        for c in flags.chars() {
            match c {
                's' => on = true,
                'u' => off = true,
                'q' => quiet = true,
                'p' => print = true,
                'o' => set_o = true,
                _ => {
                    emit(format!("shopt: -{c}: invalid option"));
                    emit("shopt: usage: shopt [-pqsu] [-o] [optname ...]");
                    return Err(BuiltinFailed(ExitStatus(2)).into());
                }
            }
        }
        names = rest;
    }
    if on && off {
        emit("shopt: cannot set and unset shell options simultaneously");
        return Err(BuiltinFailed(ExitStatus(1)).into());
    }
    if set_o {
        let mut args: Vec<String> = Vec::new();
        if on || off {
            for name in names {
                args.push(if on { "-o" } else { "+o" }.to_owned());
                args.push((*name).to_owned());
            }
        } else {
            args.push("+o".to_owned());
        }
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        return super::env::builtin_set(shell, &refs);
    }

    let listed: Vec<&str> = if names.is_empty() {
        NAMES.to_vec()
    } else {
        names.to_vec()
    };
    let setting = (on || off) && !names.is_empty();
    let mut status = ExitStatus::SUCCESS;
    for name in listed {
        if setting {
            if !set(shell, name, on) {
                emit(format!("shopt: {name}: invalid shell option name"));
                status = ExitStatus::FAILURE;
            }
            continue;
        }
        let Some(value) = value(shell, name) else {
            emit(format!("shopt: {name}: invalid shell option name"));
            status = ExitStatus::FAILURE;
            continue;
        };
        if !value {
            status = ExitStatus::FAILURE;
        }
        // Listing everything shows only the on ones with `-s`, the off
        // ones with `-u`.
        if quiet || (names.is_empty() && ((on && !value) || (off && value))) {
            continue;
        }
        if print {
            shell.outln(&format!("shopt {} {name}", if value { "-s" } else { "-u" }))?;
        } else {
            shell.outln(&format!("{name:<15}\t{}", if value { "on" } else { "off" }))?;
        }
    }
    // Listing every option is not a question about any one of them.
    if names.is_empty() && !quiet {
        return Ok(ExitStatus::SUCCESS);
    }
    Ok(status)
}
