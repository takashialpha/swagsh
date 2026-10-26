use anyhow::Result;
use clap::Parser;

use crate::errfmt::emit;
use crate::eval::Shell;
use crate::jobs::ExitStatus;

use super::Builtin;
use super::help::{Example, HandParsed, Help, HelpPage};

#[derive(Parser)]
#[command(name = "unset", about = "Unset variables and/or functions")]
pub struct UnsetBuiltin {
    /// Treat each NAME as a variable only
    #[arg(short = 'v', conflicts_with = "function")]
    variable: bool,
    /// Treat each NAME as a function only
    #[arg(short = 'f')]
    function: bool,
    /// Unset a reference variable itself, not the variable it refers to
    #[arg(short = 'n')]
    nameref: bool,
    /// The variables or functions to remove
    names: Vec<String>,
}

impl Builtin for UnsetBuiltin {
    fn run(self, shell: &mut Shell) -> Result<ExitStatus> {
        let mut status = ExitStatus::SUCCESS;
        for name in &self.names {
            if !self.function
                && let Some((array, rest)) = name.split_once('[')
                && let Some(index) = rest.strip_suffix(']')
            {
                if let Err(e) = unset_element(shell, array, index) {
                    emit(format!("unset: {e}"));
                    status = ExitStatus::FAILURE;
                }
                continue;
            }
            if self.nameref {
                shell.env.unset_ref(name);
            } else if self.function {
                shell.env.unset_function(name);
            } else if shell.env.is_readonly(name) {
                emit(format!(
                    "unset: {}",
                    crate::env::ReadonlyError(name.clone())
                ));
                status = ExitStatus::FAILURE;
            } else if self.variable {
                shell.env.unset_var(name);
            } else {
                shell.env.unset(name);
            }
        }
        if status.is_success() {
            Ok(status)
        } else {
            Err(super::BuiltinFailed(status).into())
        }
    }
}

/// `unset 'name[index]'`: one element, or with `@` or `*`, the whole array.
fn unset_element(shell: &mut Shell, name: &str, index: &str) -> Result<()> {
    if !crate::expand::is_name(name) {
        anyhow::bail!("`{name}[{index}]': not a valid identifier");
    }
    if index == "@" || index == "*" {
        if shell.env.is_readonly(name) {
            anyhow::bail!(crate::env::ReadonlyError(name.to_owned()));
        }
        shell.env.unset_var(name);
        return Ok(());
    }
    let key = shell.key_for(name, index)?;
    shell.env.unset_element(name, &key)?;
    Ok(())
}

/// Not wired into `BUILTINS` via `dispatch::<SetOperandsBuiltin>` directly
/// (unlike almost every other clap-backed builtin) because `set` is really
/// three different jobs sharing one name, and clap alone can't do the
/// leading-flags part at all, let alone tell all three apart:
///
/// - `set` with no arguments at all prints every variable.
/// - `set -e`/`-x`/`-u`/`-o name`/`+e`/... (mixed `-`/`+` prefixes, toggling
///   `errexit`/`xtrace`/`nounset`) has no clap equivalent: clap has no
///   concept of a `+`-prefixed flag at all, so this is hand-scanned by
///   `apply_set_flags` below, updating `shell` directly as a side effect
///   before any clap parsing happens.
/// - Whatever's left after that (real operands, or a lone `--`) replaces
///   the positional parameters, and *that* part does go through clap
///   normally, via `SetOperandsBuiltin`.
///
/// `set --` (an explicit `--` followed by zero operands) clears the
/// positional parameters, while `set -e` alone (flags, no `--`, nothing
/// after) does not touch them at all: both reduce to "nothing left" after
/// flag-scanning, so `apply_set_flags` reports whether it actually saw a
/// `--` to tell the two apart.
pub struct Set;

impl Help for Set {
    fn help() -> HelpPage {
        let mut options = vec![(
            "-o OPTION".to_owned(),
            "Turn on OPTION, by its long name".to_owned(),
        )];
        for o in OPTIONS {
            options.push(o.letter.map_or_else(
                || (format!("-o {}", o.name), o.what.to_owned()),
                |l| (format!("-{l}"), format!("{} ({})", o.what, o.name)),
            ));
        }
        let options: Vec<(&str, &str)> = options
            .iter()
            .map(|(f, w)| (f.as_str(), w.as_str()))
            .collect();
        HelpPage::manual(
            "Set shell options and the positional parameters",
            "set [-abCefHmnuvx] [-o OPTION] [--] [ARG...]",
            &options,
            &[
                Example {
                    command: "set -eu",
                    meaning: "Exit on a failed command or an unset variable",
                },
                Example {
                    command: "set -- a b",
                    meaning: "Make $1 a and $2 b",
                },
                Example {
                    command: "set +o",
                    meaning: "Print the commands that restore the current options",
                },
            ],
            &[
                "`+` instead of `-` turns an option off.",
                "With no arguments, prints every variable. `set --` clears $1, $2, ...",
            ],
        )
    }
}

impl HandParsed for Set {
    fn run(shell: &mut Shell, args: &[&str]) -> Result<ExitStatus> {
        builtin_set(shell, args)
    }
}

fn builtin_set(shell: &mut Shell, args: &[&str]) -> Result<ExitStatus> {
    if args.is_empty() {
        return print_all_vars(shell);
    }
    let (remaining, saw_double_dash) = match apply_set_flags(shell, args) {
        Ok(r) => r,
        Err(e) => {
            emit(e);
            return Err(super::BuiltinFailed(ExitStatus(2)).into());
        }
    };
    if remaining.is_empty() && !saw_double_dash {
        // e.g. `set -e` alone: flags applied, positional params untouched.
        return Ok(ExitStatus::SUCCESS);
    }
    super::cli::dispatch::<SetOperandsBuiltin>(shell, remaining)
}

fn print_all_vars(shell: &mut Shell) -> Result<ExitStatus> {
    let lines: Vec<String> = shell
        .env
        .declared_names()
        .into_iter()
        .filter_map(|name| super::declare::assignment_text(shell, name))
        .collect();
    for line in lines {
        shell.outln(&line)?;
    }
    Ok(ExitStatus::SUCCESS)
}

/// Scans `args` for leading `-`/`+`-prefixed option flags, applying each to
/// `shell` as it's recognized, and returns whatever's left (to be treated
/// as operands) plus whether a `--` was the thing that ended the scan.
///
/// The POSIX options this shell implements are `-a`, `-b`, `-C`, `-e`,
/// `-f`, `-m`, `-n`, `-u`, `-v` and `-x`, and bash's `-H`, plus their
/// `-o`/`+o` long names
/// (see [`OPTIONS`]). Accepting-and-ignoring the rest (`-h`, ...) would be
/// a fake flag pretending to work, so an unrecognized one is a usage error
/// (`invalid option`, exit 2) rather than a silent no-op.
fn apply_set_flags<'a>(shell: &mut Shell, args: &'a [&'a str]) -> Result<(&'a [&'a str], bool)> {
    let mut i = 0;
    let mut saw_double_dash = false;
    while i < args.len() {
        let arg = args[i];
        if arg == "--" {
            saw_double_dash = true;
            i += 1;
            break;
        }
        if arg == "-" {
            // Assign the rest to positional params; also turns off -x (the
            // only one of the flags this bare form also disables that we
            // implement).
            shell.xtrace = false;
            i += 1;
            break;
        }
        if arg == "+" {
            i += 1;
            break;
        }
        let enable = match arg.as_bytes().first() {
            Some(b'-') => true,
            Some(b'+') => false,
            _ => break,
        };
        let rest = &arg[1..];
        if rest == "o" {
            i += 1;
            let Some(&name) = args.get(i) else {
                print_set_o_status(shell, enable)?;
                return Ok((&[], false));
            };
            let option = OPTIONS
                .iter()
                .find(|o| o.name == name)
                .ok_or_else(|| anyhow::anyhow!("set: {name}: invalid option name"))?;
            match option.letter {
                Some(letter) => set_option(shell, letter, enable),
                None => set_named_option(shell, name, enable),
            }
            i += 1;
            continue;
        }
        for c in rest.chars() {
            if !OPTIONS.iter().any(|o| o.letter == Some(c)) {
                anyhow::bail!(
                    "set: {arg}: invalid option\nset: usage: set [-abCefHmnuvx] [-o option] [--] [arg ...]"
                );
            }
            set_option(shell, c, enable);
        }
        i += 1;
    }
    Ok((&args[i..], saw_double_dash))
}

struct ShellOption {
    /// `None` for an option with only a long name (`pipefail`).
    letter: Option<char>,
    name: &'static str,
    /// What it does, for `help set`.
    what: &'static str,
}

/// Every `set` option this shell implements, in `set -o` listing order.
const OPTIONS: &[ShellOption] = &[
    ShellOption {
        letter: Some('a'),
        name: "allexport",
        what: "Export every variable that is assigned",
    },
    ShellOption {
        letter: None,
        name: "emacs",
        what: "Emacs keys at the prompt (the opposite of `vi`)",
    },
    ShellOption {
        letter: Some('e'),
        name: "errexit",
        what: "Exit when a command fails",
    },
    ShellOption {
        letter: Some('H'),
        name: "histexpand",
        what: "Expand `!` history references at the prompt",
    },
    ShellOption {
        letter: Some('m'),
        name: "monitor",
        what: "Job control: run jobs in their own process groups",
    },
    ShellOption {
        letter: Some('C'),
        name: "noclobber",
        what: "Don't let `>` overwrite an existing file",
    },
    ShellOption {
        letter: Some('n'),
        name: "noexec",
        what: "Read commands without running them",
    },
    ShellOption {
        letter: Some('f'),
        name: "noglob",
        what: "Don't expand pathname patterns",
    },
    ShellOption {
        letter: Some('b'),
        name: "notify",
        what: "Report finished jobs at once, not at the next prompt",
    },
    ShellOption {
        letter: Some('u'),
        name: "nounset",
        what: "Treat expanding an unset variable as an error",
    },
    ShellOption {
        letter: None,
        name: "pipefail",
        what: "A pipeline fails when any of its commands fails",
    },
    ShellOption {
        letter: Some('v'),
        name: "verbose",
        what: "Print each command as it is read",
    },
    ShellOption {
        letter: None,
        name: "vi",
        what: "Vi keys at the prompt",
    },
    ShellOption {
        letter: Some('x'),
        name: "xtrace",
        what: "Print each command, expanded, before running it",
    },
];

fn set_option(shell: &mut Shell, letter: char, enable: bool) {
    match letter {
        'a' => shell.env.allexport = enable,
        'b' => shell.notify = enable,
        'H' => shell.histexpand = enable,
        'C' => shell.noclobber = enable,
        'e' => shell.errexit = enable,
        'f' => shell.noglob = enable,
        'm' => shell.set_monitor(enable),
        'n' => shell.noexec = enable,
        'u' => shell.nounset = enable,
        'v' => shell.verbose = enable,
        'x' => shell.xtrace = enable,
        _ => {}
    }
}

/// Sets an option that has only a long name.
fn set_named_option(shell: &mut Shell, name: &str, enable: bool) {
    match name {
        "pipefail" => shell.pipefail = enable,
        "vi" => shell.vi = Some(enable),
        "emacs" => shell.vi = Some(!enable),
        _ => {}
    }
}

/// Turns each `set` option on or off as `flags` (as in `$-`) says, for
/// `local -` when its function returns.
pub fn restore_options(shell: &mut Shell, flags: &str) {
    let current = shell.option_flags();
    for letter in OPTIONS.iter().filter_map(|o| o.letter) {
        let enable = flags.contains(letter);
        if current.contains(letter) != enable {
            set_option(shell, letter, enable);
        }
    }
}

/// Whether the `set -o` option named `name` is on (`test -o name`).
pub(super) fn option_is_on(shell: &Shell, name: &str) -> bool {
    OPTIONS
        .iter()
        .find(|o| o.name == name)
        .is_some_and(|o| option_enabled(shell, o))
}

fn option_enabled(shell: &Shell, option: &ShellOption) -> bool {
    match (option.letter, option.name) {
        (Some(letter), _) => shell.option_flags().contains(letter),
        (None, "vi") => shell.vi == Some(true),
        (None, "emacs") => shell.vi != Some(true),
        (None, _) => shell.pipefail,
    }
}

/// `on` or `off`, in green or dim with `color`.
pub(super) fn on_off(on: bool, color: bool) -> std::borrow::Cow<'static, str> {
    use crate::paint::{DIM, GOOD, paint};
    if on {
        paint("on", GOOD, color)
    } else {
        paint("off", DIM, color)
    }
}

/// `set -o` lists every option and whether it is on; `set +o` prints the
/// `set` commands that would restore the current settings.
fn print_set_o_status(shell: &mut Shell, human: bool) -> Result<()> {
    let color = crate::paint::stdout(shell);
    for option in OPTIONS {
        let on = option_enabled(shell, option);
        if human {
            shell.outln(&format!("{:<15}\t{}", option.name, on_off(on, color)))?;
        } else {
            shell.outln(&format!(
                "set {}o {}",
                if on { '-' } else { '+' },
                option.name
            ))?;
        }
    }
    Ok(())
}

#[derive(Parser)]
#[command(
    name = "set",
    about = "Replace the positional parameters",
    trailing_var_arg = true
)]
pub struct SetOperandsBuiltin {
    #[arg(allow_hyphen_values = true)]
    operands: Vec<String>,
}

impl Builtin for SetOperandsBuiltin {
    fn run(self, shell: &mut Shell) -> Result<ExitStatus> {
        shell.env.set_positional_args(self.operands);
        Ok(ExitStatus::SUCCESS)
    }
}

#[derive(Parser)]
#[command(
    name = "shift",
    about = "Shift positional parameters",
    allow_negative_numbers = true
)]
pub struct ShiftBuiltin {
    /// How many positional parameters to drop (1 when left out)
    count: Option<i64>,
}

impl Builtin for ShiftBuiltin {
    fn run(self, shell: &mut Shell) -> Result<ExitStatus> {
        let mut pos = shell.env.positional_args().to_vec();
        // POSIX: a count out of range is an error, which as `shift` is a
        // special builtin ends a non-interactive shell.
        let count = self.count.unwrap_or(1);
        let n = usize::try_from(count)
            .ok()
            .filter(|&n| n <= pos.len())
            .ok_or_else(|| anyhow::anyhow!("shift: {count}: shift count out of range"))?;
        pos.drain(..n);
        shell.env.set_positional_args(pos);
        Ok(ExitStatus::SUCCESS)
    }
}
