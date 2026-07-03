//! Control-flow builtins: the trivial no-ops (`:`/`true`/`false`) and the
//! signals that unwind through `run_command`'s `anyhow::Error` channel
//! (`break`/`continue`/`return`/`exit`). `eval`/`exec`/`source` (dynamic
//! execution rather than control flow) live in `script.rs`.

use anyhow::{Error, Result};
use clap::Parser;

use crate::errfmt::emit;
use crate::eval::{LoopSignal, ReturnSignal, Shell};
use crate::jobs::ExitStatus;

use super::Builtin;

// `:`/`true`/`false` stay plain functions, not clap: they ignore every
// argument unconditionally, `--help` included (`true --help` exits 0
// silently, and `false --help` exits 1: a clap wrapper's generic `--help`
// handling always returns 0, which would be a real, wrong exit code for
// `false`, not just different text). Their entire job is to do nothing
// with whatever they're given.

#[allow(clippy::unnecessary_wraps)] // required by BuiltinFn signature
pub const fn builtin_colon(_: &mut Shell, _: &[&str]) -> Result<ExitStatus> {
    Ok(ExitStatus::SUCCESS)
}

#[allow(clippy::unnecessary_wraps)] // required by BuiltinFn signature
pub const fn builtin_true(_: &mut Shell, _: &[&str]) -> Result<ExitStatus> {
    Ok(ExitStatus::SUCCESS)
}

#[allow(clippy::unnecessary_wraps)] // required by BuiltinFn signature
pub const fn builtin_false(_: &mut Shell, _: &[&str]) -> Result<ExitStatus> {
    Ok(ExitStatus::FAILURE)
}

/// Not inside any loop (of the current function call) is only a warning,
/// not an abort: it prints the message and execution just continues on to
/// the next statement, exit status 0.
fn not_in_loop(name: &str) -> Result<ExitStatus> {
    emit(format!(
        "{name}: only meaningful in a `for', `while', or `until' loop"
    ));
    Ok(ExitStatus::SUCCESS)
}

/// `break`/`continue`'s `N` (how many enclosing loops to unwind) has to be a
/// positive integer; `clap::value_parser!(u32).range(1..)` rejects `0`,
/// negative numbers, and non-numeric tokens uniformly, the same UsageError
/// path (exit 2, nothing unwound) every other clap-backed builtin's bad
/// argument already takes. Bash instead special-cases these as "still
/// unwind one level despite the error," a real (minor) divergence, traded
/// for not hand-parsing this at all.
#[derive(Parser)]
#[command(
    name = "break",
    about = "Exit from an enclosing for, while, or until loop",
    allow_negative_numbers = true
)]
pub struct BreakBuiltin {
    #[arg(value_parser = clap::value_parser!(u32).range(1..))]
    level: Option<u32>,
}

impl Builtin for BreakBuiltin {
    fn run(self, shell: &mut Shell) -> Result<ExitStatus> {
        if shell.loop_depth == 0 {
            return not_in_loop("break");
        }
        let n = self.level.unwrap_or(1).min(shell.loop_depth);
        Err(Error::new(LoopSignal::Break(n)))
    }
}

#[derive(Parser)]
#[command(
    name = "continue",
    about = "Resume the next iteration of an enclosing for, while, or until loop",
    allow_negative_numbers = true
)]
pub struct ContinueBuiltin {
    #[arg(value_parser = clap::value_parser!(u32).range(1..))]
    level: Option<u32>,
}

impl Builtin for ContinueBuiltin {
    fn run(self, shell: &mut Shell) -> Result<ExitStatus> {
        if shell.loop_depth == 0 {
            return not_in_loop("continue");
        }
        let n = self.level.unwrap_or(1).min(shell.loop_depth);
        Err(Error::new(LoopSignal::Continue(n)))
    }
}

#[derive(Parser)]
#[command(
    name = "return",
    about = "Return from a shell function or sourced script",
    allow_negative_numbers = true
)]
pub struct ReturnBuiltin {
    code: Option<i32>,
}

impl Builtin for ReturnBuiltin {
    fn run(self, shell: &mut Shell) -> Result<ExitStatus> {
        let code = self.code.unwrap_or(shell.last_status.0);
        Err(Error::new(ReturnSignal(code)))
    }
}

#[derive(Parser)]
#[command(name = "exit", about = "Exit the shell", allow_negative_numbers = true)]
pub struct ExitBuiltin {
    code: Option<i32>,
}

impl Builtin for ExitBuiltin {
    fn run(self, _shell: &mut Shell) -> Result<ExitStatus> {
        std::process::exit(self.code.unwrap_or(0));
    }
}
