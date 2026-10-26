//! Control-flow builtins: the trivial no-ops (`:`/`true`/`false`) and the
//! signals that unwind through `run_command`'s `anyhow::Error` channel.
//!
//! (`break`/`continue`/`return`/`exit`). `eval`/`exec`/`source` (dynamic
//! execution rather than control flow) live in `script.rs`.

use anyhow::{Error, Result};
use clap::Parser;

use crate::errfmt::emit;
use crate::eval::{LoopSignal, ReturnSignal, Shell};
use crate::jobs::ExitStatus;

use super::Builtin;
use super::help::{Example, HandParsed, Help, HelpPage};

// `:`/`true`/`false` are hand-parsed, not clap: they ignore every argument
// unconditionally, `--help` included (`true --help` exits 0 silently, and
// `false --help` exits 1: a clap wrapper's generic `--help` handling always
// returns 0, which would be a real, wrong exit code for `false`, not just
// different text). Their entire job is to do nothing with whatever they're
// given.

pub struct Colon;

impl Help for Colon {
    fn help() -> HelpPage {
        HelpPage::manual(
            "Do nothing, successfully",
            ": [ARG...]",
            &[],
            &[Example {
                command: ": \"${dir:=/tmp}\"",
                meaning: "Set dir to /tmp unless it is set already",
            }],
            &["Its arguments are still expanded, which is what makes it useful."],
        )
    }
}

impl HandParsed for Colon {
    const HELP_FLAG: bool = false;

    fn run(_: &mut Shell, _: &[&str]) -> Result<ExitStatus> {
        Ok(ExitStatus::SUCCESS)
    }
}

pub struct True;

impl Help for True {
    fn help() -> HelpPage {
        HelpPage::manual("Do nothing, successfully", "true", &[], &[], &[])
    }
}

impl HandParsed for True {
    const HELP_FLAG: bool = false;

    fn run(_: &mut Shell, _: &[&str]) -> Result<ExitStatus> {
        Ok(ExitStatus::SUCCESS)
    }
}

pub struct False;

impl Help for False {
    fn help() -> HelpPage {
        HelpPage::manual("Do nothing, unsuccessfully", "false", &[], &[], &[])
    }
}

impl HandParsed for False {
    const HELP_FLAG: bool = false;

    fn run(_: &mut Shell, _: &[&str]) -> Result<ExitStatus> {
        Ok(ExitStatus::FAILURE)
    }
}

/// Not inside any loop (of the current function call) is only a warning,
/// not an abort: it prints the message and execution just continues on to
/// the next statement, exit status 0.
fn not_in_loop(name: &str) -> ExitStatus {
    emit(format!(
        "{name}: only meaningful in a 'for', 'while', or 'until' loop"
    ));
    ExitStatus::SUCCESS
}

/// `break`/`continue`'s `N` (how many enclosing loops to unwind) has to be a
/// positive integer.
///
/// `clap::value_parser!(u32).range(1..)` rejects `0`,
/// negative numbers, and non-numeric tokens uniformly, the same `UsageError`
/// path (exit 2, nothing unwound) every other clap-backed builtin's bad
/// argument already takes. Bash instead special-cases these as "still
/// unwind one level despite the error," a real (minor) divergence, traded
/// for not hand-parsing this at all.
#[derive(Parser)]
#[command(
    name = "break",
    about = "Exit from an enclosing for, while, or until loop",
    long_about = None,
    allow_negative_numbers = true
)]
pub struct BreakBuiltin {
    /// How many enclosing loops to leave (1 when left out)
    #[arg(value_parser = clap::value_parser!(u32).range(1..))]
    level: Option<u32>,
}

impl Builtin for BreakBuiltin {
    fn run(self, shell: &mut Shell) -> Result<ExitStatus> {
        if shell.loop_depth == 0 {
            return Ok(not_in_loop("break"));
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
    /// Which enclosing loop to continue (1, the innermost, when left out)
    #[arg(value_parser = clap::value_parser!(u32).range(1..))]
    level: Option<u32>,
}

impl Builtin for ContinueBuiltin {
    fn run(self, shell: &mut Shell) -> Result<ExitStatus> {
        if shell.loop_depth == 0 {
            return Ok(not_in_loop("continue"));
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
    /// The exit status ($? when left out)
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
    /// The exit status ($? when left out)
    code: Option<i32>,
}

impl Builtin for ExitBuiltin {
    fn run(self, shell: &mut Shell) -> Result<ExitStatus> {
        let code = self.code.unwrap_or(shell.last_status.0);
        // The REPL catches this to save history before exiting.
        if shell.interactive {
            return Err(Error::new(ExitRequest(code)));
        }
        shell.exit(code);
    }
}

/// `exit` in an interactive shell, unwinding to the REPL.
#[derive(Debug)]
pub struct ExitRequest(pub i32);

impl std::fmt::Display for ExitRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "exit {}", self.0)
    }
}

impl std::error::Error for ExitRequest {}

/// `builtin NAME [ARG...]`: runs the builtin NAME even when a function of
/// the same name would otherwise be found first.
pub struct BuiltinCmd;

impl Help for BuiltinCmd {
    fn help() -> HelpPage {
        HelpPage::manual(
            "Run a builtin, even where a function has its name",
            "builtin NAME [ARG...]",
            &[],
            &[Example {
                command: "cd() { builtin cd \"$@\" && ls; }",
                meaning: "A cd that lists the new directory",
            }],
            &[],
        )
    }
}

impl HandParsed for BuiltinCmd {
    fn run(shell: &mut Shell, args: &[&str]) -> Result<ExitStatus> {
        builtin_builtin(shell, args)
    }
}

fn builtin_builtin(shell: &mut Shell, args: &[&str]) -> Result<ExitStatus> {
    let Some((&name, rest)) = args.split_first() else {
        return Ok(ExitStatus::SUCCESS);
    };
    match crate::builtins::lookup_builtin(name) {
        Some(f) if name != "exec" => f(shell, rest),
        Some(_) => Err(anyhow::anyhow!("builtin: exec: run it directly")),
        None => Err(anyhow::anyhow!("builtin: {name}: not a shell builtin")),
    }
}

/// `let EXPR...`: evaluates each argument as `$(( ))` would, succeeding when
/// the last one is non-zero.
pub struct Let;

impl Help for Let {
    fn help() -> HelpPage {
        HelpPage::manual(
            "Evaluate arithmetic expressions",
            "let EXPRESSION...",
            &[],
            &[Example {
                command: "let 'n = n * 2' i++",
                meaning: "Double n, and add one to i",
            }],
            &[
                "Succeeds when the last expression is not zero.",
                "Each argument is one expression; quote one that has spaces.",
            ],
        )
    }
}

impl HandParsed for Let {
    fn run(shell: &mut Shell, args: &[&str]) -> Result<ExitStatus> {
        builtin_let(shell, args)
    }
}

fn builtin_let(shell: &mut Shell, args: &[&str]) -> Result<ExitStatus> {
    if args.is_empty() {
        anyhow::bail!("let: expression expected");
    }
    let mut last = 0;
    for expr in args {
        last = crate::expand::eval_arith_with(expr, &mut shell.env)
            .map_err(|e| anyhow::anyhow!("let: {e}"))?;
    }
    Ok(if last == 0 {
        ExitStatus::FAILURE
    } else {
        ExitStatus::SUCCESS
    })
}
