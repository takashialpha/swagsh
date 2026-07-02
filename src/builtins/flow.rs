use anyhow::{Error, Result};
use clap::Parser;

use crate::errfmt::strerror;
use crate::eval::{LoopSignal, ReturnSignal, Shell};
use crate::jobs::ExitStatus;

use super::Builtin;

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

#[allow(clippy::unnecessary_wraps)] // required by BuiltinFn signature
pub fn builtin_break(_: &mut Shell, _: &[&str]) -> Result<ExitStatus> {
    Err(Error::new(LoopSignal::Break))
}

#[allow(clippy::unnecessary_wraps)] // required by BuiltinFn signature
pub fn builtin_continue(_: &mut Shell, _: &[&str]) -> Result<ExitStatus> {
    Err(Error::new(LoopSignal::Continue))
}

pub fn builtin_return(shell: &mut Shell, args: &[&str]) -> Result<ExitStatus> {
    let code = args
        .first()
        .and_then(|s| s.parse::<i32>().ok())
        .unwrap_or(shell.last_status.0);
    Err(Error::new(ReturnSignal(code)))
}

pub fn builtin_exit(_: &mut Shell, args: &[&str]) -> Result<ExitStatus> {
    std::process::exit(args.first().and_then(|s| s.parse().ok()).unwrap_or(0));
}

pub fn builtin_exec(_: &mut Shell, args: &[&str]) -> Result<ExitStatus> {
    if args.is_empty() {
        return Ok(ExitStatus::SUCCESS);
    }
    let words: Vec<String> = args.iter().map(ToString::to_string).collect();
    crate::eval::Shell::do_exec(&words)?;
    unreachable!()
}

#[derive(Parser)]
#[command(
    name = "source",
    about = "Execute commands from a file in the current shell"
)]
pub struct SourceArgs {
    /// Search PATH (colon-separated) for FILENAME instead of $PATH
    #[arg(short = 'p')]
    path: Option<String>,
    filename: String,
    arguments: Vec<String>,
}

/// Resolves `filename` the way bash's `source` does: used as-is if it
/// contains a `/` or exists relative to the current directory, otherwise
/// searched for as a bare name across `search_path` (`-p`'s argument, or
/// `$PATH` if `-p` wasn't given) the same way an external command is.
fn resolve_source_path(filename: &str, search_path: Option<&str>, shell: &Shell) -> String {
    if filename.contains('/') || std::path::Path::new(filename).exists() {
        return filename.to_owned();
    }
    let search_path = search_path
        .map(str::to_owned)
        .or_else(|| shell.env.get("PATH"))
        .unwrap_or_default();
    for dir in search_path.split(':') {
        let candidate = format!("{dir}/{filename}");
        if std::path::Path::new(&candidate).exists() {
            return candidate;
        }
    }
    filename.to_owned()
}

impl Builtin for SourceArgs {
    fn run(self, shell: &mut Shell) -> Result<ExitStatus> {
        let resolved = resolve_source_path(&self.filename, self.path.as_deref(), shell);
        let src = std::fs::read_to_string(&resolved)
            .map_err(|e| anyhow::anyhow!("source: {resolved}: {}", strerror(e)))?;
        let program =
            crate::parser::parse(&src).map_err(|e| anyhow::anyhow!("source: {resolved}: {e}"))?;

        // Extra arguments become $1, $2, ... only for this run, restored
        // afterward; with none given, the caller's own positional params
        // stay visible to the sourced script, same as bash.
        let old_args = (!self.arguments.is_empty()).then(|| {
            let old = shell.env.positional_args().to_vec();
            shell.env.set_positional_args(self.arguments.clone());
            old
        });
        let result = shell.run_program(&program);
        if let Some(old) = old_args {
            shell.env.set_positional_args(old);
        }
        result
    }
}
