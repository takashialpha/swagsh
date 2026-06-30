use anyhow::{Result, bail};

use crate::eval::Shell;
use crate::jobs::ExitStatus;

#[allow(clippy::unnecessary_wraps)] // required by BuiltinFn signature
pub fn builtin_export(shell: &mut Shell, args: &[&str]) -> Result<ExitStatus> {
    if args.is_empty() {
        for (k, v) in shell.env.exported() {
            println!("export {k}={v}");
        }
        return Ok(ExitStatus::SUCCESS);
    }
    for arg in args {
        if let Some((k, v)) = arg.split_once('=') {
            shell.env.export(k, v.to_owned());
        } else {
            shell.env.mark_exported(arg);
        }
    }
    Ok(ExitStatus::SUCCESS)
}

#[allow(clippy::unnecessary_wraps)] // required by BuiltinFn signature
pub fn builtin_unset(shell: &mut Shell, args: &[&str]) -> Result<ExitStatus> {
    for arg in args {
        shell.env.unset(arg);
    }
    Ok(ExitStatus::SUCCESS)
}

#[allow(clippy::unnecessary_wraps)] // required by BuiltinFn signature
pub fn builtin_set(shell: &mut Shell, args: &[&str]) -> Result<ExitStatus> {
    if args.is_empty() {
        for (k, v) in shell.env.all_vars() {
            println!("{k}={v}");
        }
        return Ok(ExitStatus::SUCCESS);
    }
    if args.first() == Some(&"--") {
        shell
            .env
            .set_positional_args(args[1..].iter().map(ToString::to_string).collect());
    }
    Ok(ExitStatus::SUCCESS)
}

pub fn builtin_shift(shell: &mut Shell, args: &[&str]) -> Result<ExitStatus> {
    let n: usize = args.first().and_then(|s| s.parse().ok()).unwrap_or(1);
    let mut pos = shell.env.positional_args().to_vec();
    if n > pos.len() {
        bail!("shift: shift count out of range");
    }
    pos.drain(..n);
    shell.env.set_positional_args(pos);
    Ok(ExitStatus::SUCCESS)
}
