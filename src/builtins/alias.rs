use anyhow::Result;

use crate::eval::Shell;
use crate::jobs::ExitStatus;

#[allow(clippy::unnecessary_wraps)] // required by BuiltinFn signature
pub fn builtin_alias(shell: &mut Shell, args: &[&str]) -> Result<ExitStatus> {
    if args.is_empty() {
        let mut pairs: Vec<(&str, &str)> = shell.env.all_aliases().collect();
        pairs.sort_by_key(|(k, _)| *k);
        for (k, v) in pairs {
            println!("alias {k}='{v}'");
        }
        return Ok(ExitStatus::SUCCESS);
    }
    for arg in args {
        if let Some((k, v)) = arg.split_once('=') {
            shell.env.set_alias(k.to_owned(), v.to_owned());
        } else if let Some(v) = shell.env.get_alias(arg) {
            println!("alias {arg}='{v}'");
        } else {
            eprintln!("swagsh: alias: {arg}: not found");
            return Ok(ExitStatus::FAILURE);
        }
    }
    Ok(ExitStatus::SUCCESS)
}

#[allow(clippy::unnecessary_wraps)] // required by BuiltinFn signature
pub fn builtin_unalias(shell: &mut Shell, args: &[&str]) -> Result<ExitStatus> {
    if args.first() == Some(&"-a") {
        shell.env.clear_aliases();
        return Ok(ExitStatus::SUCCESS);
    }
    for arg in args {
        shell.env.remove_alias(arg);
    }
    Ok(ExitStatus::SUCCESS)
}
