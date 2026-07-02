use anyhow::{Result, bail};
use clap::Parser;

use crate::eval::Shell;
use crate::expand::shell_quote;
use crate::jobs::ExitStatus;

use super::Builtin;

#[derive(Parser)]
#[command(
    name = "export",
    about = "Mark variables for export to child processes"
)]
pub struct ExportArgs {
    /// Remove the export attribute instead of setting it
    #[arg(short = 'n')]
    remove: bool,
    names: Vec<String>,
}

impl Builtin for ExportArgs {
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
pub struct UnsetArgs {
    /// Treat each NAME as a variable only
    #[arg(short = 'v', overrides_with = "function")]
    variable: bool,
    /// Treat each NAME as a function only
    #[arg(short = 'f', overrides_with = "variable")]
    function: bool,
    names: Vec<String>,
}

impl Builtin for UnsetArgs {
    fn run(self, shell: &mut Shell) -> Result<ExitStatus> {
        let function = self.function && !self.variable;
        for name in &self.names {
            if function {
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

#[allow(clippy::unnecessary_wraps)] // required by BuiltinFn signature
pub fn builtin_set(shell: &mut Shell, args: &[&str]) -> Result<ExitStatus> {
    if args.is_empty() {
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
