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
            let printed_any = !pairs.is_empty();
            for (k, v) in pairs {
                println!("export {k}={}", shell_quote(&v));
            }
            if printed_any {
                shell.note_stdout("\n");
            }
            return Ok(ExitStatus::SUCCESS);
        }
        for name in &self.names {
            if self.remove {
                shell.env.unexport(name);
            } else if let Some((k, v)) = name.split_once('=') {
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
        for name in &self.names {
            if self.function {
                shell.env.unset_function(name);
            } else if self.variable {
                shell.env.unset_var(name);
            } else {
                shell.env.unset(name);
            }
        }
        Ok(ExitStatus::SUCCESS)
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
        return Ok(print_all_vars(shell));
    }
    let (remaining, saw_double_dash) = match apply_set_flags(shell, args) {
        Ok(r) => r,
        Err(e) => {
            emit(e);
            return Ok(ExitStatus(2));
        }
    };
    if remaining.is_empty() && !saw_double_dash {
        // e.g. `set -e` alone: flags applied, positional params untouched.
        return Ok(ExitStatus::SUCCESS);
    }
    super::cli::dispatch::<SetOperandsBuiltin>(shell, remaining)
}

fn print_all_vars(shell: &mut Shell) -> ExitStatus {
    let pairs: Vec<(String, String)> = shell
        .env
        .all_vars()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
    let printed_any = !pairs.is_empty();
    for (k, v) in pairs {
        println!("{k}={}", shell_quote(&v));
    }
    if printed_any {
        shell.note_stdout("\n");
    }
    ExitStatus::SUCCESS
}

/// Scans `args` for leading `-`/`+`-prefixed option flags, applying each to
/// `shell` as it's recognized, and returns whatever's left (to be treated
/// as operands) plus whether a `--` was the thing that ended the scan.
///
/// Only `-e`/`-x`/`-u` (`errexit`/`xtrace`/`nounset`) and their `-o`/`+o`
/// long-option spellings are real here. There are roughly fifteen more
/// well-known shell options (`-a`, `-b`, `-f`, `-m`, `-o pipefail`, ...)
/// this shell doesn't implement; accepting-and-ignoring them would be a
/// fake flag pretending to work, so an unrecognized one is a usage error
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
                print_set_o_status(shell);
                return Ok((&[], false));
            };
            apply_o_option(shell, name, enable)?;
            i += 1;
            continue;
        }
        for c in rest.chars() {
            match c {
                'e' => shell.errexit = enable,
                'x' => shell.xtrace = enable,
                'u' => shell.nounset = enable,
                _ => anyhow::bail!(
                    "set: {arg}: invalid option\nset: usage: set [-eux] [-o option] [--] [arg ...]"
                ),
            }
        }
        i += 1;
    }
    Ok((&args[i..], saw_double_dash))
}

fn apply_o_option(shell: &mut Shell, name: &str, enable: bool) -> Result<()> {
    match name {
        "errexit" => shell.errexit = enable,
        "xtrace" => shell.xtrace = enable,
        "nounset" => shell.nounset = enable,
        _ => anyhow::bail!("set: {name}: invalid option name"),
    }
    Ok(())
}

fn print_set_o_status(shell: &Shell) {
    for (name, val) in [
        ("errexit", shell.errexit),
        ("nounset", shell.nounset),
        ("xtrace", shell.xtrace),
    ] {
        println!("{name:<15}{}", if val { "on" } else { "off" });
    }
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
