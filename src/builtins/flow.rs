use anyhow::{Error, Result};

use crate::eval::{LoopSignal, ReturnSignal, Shell};
use crate::jobs::ExitStatus;

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

pub fn builtin_source(shell: &mut Shell, args: &[&str]) -> Result<ExitStatus> {
    let path = args
        .first()
        .ok_or_else(|| anyhow::anyhow!("source: filename required"))?;
    let src = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("source: {path}: {e}"))?;
    let program = crate::parser::parse(&src).map_err(|e| anyhow::anyhow!("source: {path}: {e}"))?;
    shell.run_program(&program)
}
