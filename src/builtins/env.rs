use anyhow::Result;
use clap::Parser;

use crate::errfmt::emit;
use crate::eval::Shell;
use crate::expand::shell_quote;
use crate::jobs::ExitStatus;

use super::Builtin;

#[derive(Parser)]
#[command(
    name = "export",
    about = "Mark variables for export to child processes"
)]
pub struct ExportBuiltin {
    /// Remove the export attribute instead of setting it
    #[arg(short = 'n')]
    remove: bool,
    names: Vec<String>,
}

impl Builtin for ExportBuiltin {
    fn run(self, shell: &mut Shell) -> Result<ExitStatus> {
        if self.names.is_empty() {
            let pairs: Vec<(String, String)> = shell
                .env
                .exported()
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect();
            for (k, v) in pairs {
                shell.outln(&format!("export {k}={}", shell_quote(&v)))?;
            }
            return Ok(ExitStatus::SUCCESS);
        }
        for name in &self.names {
            if self.remove {
                shell.env.unexport(name);
            } else if let Some((k, v)) = name.split_once('=') {
                if shell.env.is_readonly(k) {
                    anyhow::bail!("export: {}", crate::env::ReadonlyError(k.to_owned()));
                }
                shell.env.export(k, v.to_owned());
            } else {
                shell.env.mark_exported(name);
            }
        }
        Ok(ExitStatus::SUCCESS)
    }
}

#[derive(Parser)]
#[command(name = "unset", about = "Unset variables and/or functions")]
pub struct UnsetBuiltin {
    /// Treat each NAME as a variable only
    #[arg(short = 'v', conflicts_with = "function")]
    variable: bool,
    /// Treat each NAME as a function only
    #[arg(short = 'f')]
    function: bool,
    names: Vec<String>,
}

impl Builtin for UnsetBuiltin {
    fn run(self, shell: &mut Shell) -> Result<ExitStatus> {
        let mut status = ExitStatus::SUCCESS;
        for name in &self.names {
            if self.function {
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
#[allow(clippy::unnecessary_wraps)] // required by BuiltinFn signature
pub fn builtin_set(shell: &mut Shell, args: &[&str]) -> Result<ExitStatus> {
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
    let pairs: Vec<(String, String)> = shell
        .env
        .all_vars()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
    for (k, v) in pairs {
        shell.outln(&format!("{k}={}", shell_quote(&v)))?;
    }
    Ok(ExitStatus::SUCCESS)
}

/// Scans `args` for leading `-`/`+`-prefixed option flags, applying each to
/// `shell` as it's recognized, and returns whatever's left (to be treated
/// as operands) plus whether a `--` was the thing that ended the scan.
///
/// The POSIX options this shell implements are `-a`, `-C`, `-e`, `-f`,
/// `-m`, `-n`, `-u`, `-v` and `-x`, plus their `-o`/`+o` long names (see
/// [`OPTIONS`]). Accepting-and-ignoring the rest (`-b`, ...) would be
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
            set_option(shell, option.letter, enable);
            i += 1;
            continue;
        }
        for c in rest.chars() {
            if !OPTIONS.iter().any(|o| o.letter == c) {
                anyhow::bail!(
                    "set: {arg}: invalid option\nset: usage: set [-aCefmnuvx] [-o option] [--] [arg ...]"
                );
            }
            set_option(shell, c, enable);
        }
        i += 1;
    }
    Ok((&args[i..], saw_double_dash))
}

struct ShellOption {
    letter: char,
    name: &'static str,
}

/// Every `set` option this shell implements, in `set -o` listing order.
const OPTIONS: &[ShellOption] = &[
    ShellOption {
        letter: 'a',
        name: "allexport",
    },
    ShellOption {
        letter: 'e',
        name: "errexit",
    },
    ShellOption {
        letter: 'm',
        name: "monitor",
    },
    ShellOption {
        letter: 'C',
        name: "noclobber",
    },
    ShellOption {
        letter: 'n',
        name: "noexec",
    },
    ShellOption {
        letter: 'f',
        name: "noglob",
    },
    ShellOption {
        letter: 'u',
        name: "nounset",
    },
    ShellOption {
        letter: 'v',
        name: "verbose",
    },
    ShellOption {
        letter: 'x',
        name: "xtrace",
    },
];

fn set_option(shell: &mut Shell, letter: char, enable: bool) {
    match letter {
        'a' => shell.env.allexport = enable,
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

fn option_enabled(shell: &Shell, letter: char) -> bool {
    shell.option_flags().contains(letter)
}

/// `set -o` lists every option and whether it is on; `set +o` prints the
/// `set` commands that would restore the current settings.
fn print_set_o_status(shell: &mut Shell, human: bool) -> Result<()> {
    for option in OPTIONS {
        let on = option_enabled(shell, option.letter);
        if human {
            shell.outln(&format!(
                "{:<15}{}",
                option.name,
                if on { "on" } else { "off" }
            ))?;
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
    count: Option<i64>,
}

impl Builtin for ShiftBuiltin {
    fn run(self, shell: &mut Shell) -> Result<ExitStatus> {
        let mut pos = shell.env.positional_args().to_vec();
        // Silently fails here (no message) for an out-of-range count,
        // negative included, unlike this builtin's other errors (a
        // non-numeric count is still a real, clap-reported usage error).
        let Ok(n) = usize::try_from(self.count.unwrap_or(1)) else {
            return Ok(ExitStatus::FAILURE);
        };
        if n > pos.len() {
            return Ok(ExitStatus::FAILURE);
        }
        pos.drain(..n);
        shell.env.set_positional_args(pos);
        Ok(ExitStatus::SUCCESS)
    }
}

#[derive(Parser)]
#[command(name = "readonly", about = "Mark variables as unchangeable")]
pub struct ReadonlyBuiltin {
    /// List readonly variables as `readonly` commands (the default with no
    /// NAMEs)
    #[arg(short = 'p')]
    print: bool,
    #[arg(value_name = "NAME[=VALUE]")]
    names: Vec<String>,
}

impl Builtin for ReadonlyBuiltin {
    fn run(self, shell: &mut Shell) -> Result<ExitStatus> {
        if self.names.is_empty() || self.print {
            for (name, value) in shell.env.readonly_vars() {
                match value {
                    Some(v) => shell.outln(&format!("readonly {name}={}", shell_quote(&v)))?,
                    None => shell.outln(&format!("readonly {name}"))?,
                }
            }
            return Ok(ExitStatus::SUCCESS);
        }
        for name in &self.names {
            let name = match name.split_once('=') {
                Some((k, v)) => {
                    shell
                        .env
                        .assign(k, v)
                        .map_err(|e| anyhow::anyhow!("readonly: {e}"))?;
                    k
                }
                None => name,
            };
            shell.env.mark_readonly(name);
        }
        Ok(ExitStatus::SUCCESS)
    }
}

#[derive(Parser)]
#[command(
    name = "local",
    about = "Make variables local to the current function",
    long_about = "Make variables local to the current function. A NAME given \
                  without a value keeps the value it had outside until assigned."
)]
pub struct LocalBuiltin {
    #[arg(value_name = "NAME[=VALUE]")]
    names: Vec<String>,
}

impl Builtin for LocalBuiltin {
    fn run(self, shell: &mut Shell) -> Result<ExitStatus> {
        for arg in &self.names {
            let (name, value) = arg
                .split_once('=')
                .map_or((arg.as_str(), None), |(k, v)| (k, Some(v)));
            if shell.env.is_readonly(name) {
                anyhow::bail!("local: {}", crate::env::ReadonlyError(name.to_owned()));
            }
            if !shell.env.make_local(name) {
                anyhow::bail!("local: can only be used in a function");
            }
            if let Some(value) = value {
                shell.env.set(name, value);
            }
        }
        Ok(ExitStatus::SUCCESS)
    }
}
