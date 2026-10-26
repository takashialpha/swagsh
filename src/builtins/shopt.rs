//! `shopt`: the shell options beyond `set`'s.

use anyhow::Result;

use super::BuiltinFailed;
use super::help::{Example, HandParsed, Help, HelpPage};
use crate::errfmt::emit;
use crate::eval::Shell;
use crate::jobs::ExitStatus;

/// Every option `shopt` knows, in the order it lists them, and what it does.
const OPTIONS: &[(&str, &str)] = &[
    (
        "autocd",
        "Typing a directory's name at the prompt changes to it",
    ),
    ("dotglob", "Patterns match names starting with `.` too"),
    ("expand_aliases", "Expand aliases (always on)"),
    (
        "extglob",
        "Extended patterns: ?(...), *(...), +(...), @(...), !(...)",
    ),
    ("failglob", "A pattern that matches no file is an error"),
    ("globstar", "`**` matches any depth of directories"),
    (
        "histappend",
        "Append to the history file on exit, not replace it",
    ),
    ("inherit_errexit", "Command substitutions keep `set -e`"),
    (
        "lastpipe",
        "Run a pipeline's last command in the shell, so `| read x` sets x",
    ),
    ("login_shell", "The shell is a login shell (read-only)"),
    ("nocaseglob", "Patterns ignore case when matching files"),
    ("nocasematch", "`case` and `[[ ]]` patterns ignore case"),
    (
        "nullglob",
        "A pattern that matches no file expands to nothing",
    ),
    (
        "sourcepath",
        "`.` and `source` look on $PATH for a name without `/`",
    ),
    ("xpg_echo", "`echo` expands backslash escapes without -e"),
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
    OPTIONS
        .iter()
        .map(|&(name, _)| name)
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
pub struct ShoptCmd;

impl Help for ShoptCmd {
    fn help() -> HelpPage {
        let mut page = HelpPage::manual(
            "Turn the shell's optional behaviors on and off",
            "shopt [-pqsu] [-o] [OPTNAME...]",
            &[
                ("-s", "Turn each OPTNAME on"),
                ("-u", "Turn each OPTNAME off"),
                ("-q", "Print nothing; succeed if every OPTNAME is on"),
                ("-p", "Print as commands that restore the settings"),
                ("-o", "OPTNAMEs are `set -o` options instead"),
            ],
            &[Example {
                command: "shopt -s globstar nullglob",
                meaning: "Let ** recurse, and empty patterns vanish",
            }],
            &["With no OPTNAME, prints every option and whether it is on."],
        );
        page.body.push('\n');
        page.body
            .push_str(&super::help::section("Option names", OPTIONS));
        page
    }
}

impl HandParsed for ShoptCmd {
    fn run(shell: &mut Shell, args: &[&str]) -> Result<ExitStatus> {
        builtin_shopt(shell, args)
    }
}

fn builtin_shopt(shell: &mut Shell, args: &[&str]) -> Result<ExitStatus> {
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
        return super::env::Set::run(shell, &refs);
    }

    let listed: Vec<&str> = if names.is_empty() {
        OPTIONS.iter().map(|&(name, _)| name).collect()
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
            shell.outln(&format!("{name:<20}\t{}", if value { "on" } else { "off" }))?;
        }
    }
    // Listing every option is not a question about any one of them.
    if names.is_empty() && !quiet {
        return Ok(ExitStatus::SUCCESS);
    }
    Ok(status)
}
