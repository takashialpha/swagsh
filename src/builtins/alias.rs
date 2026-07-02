use anyhow::Result;
use clap::Parser;

use crate::eval::Shell;
use crate::expand::shell_quote_always;
use crate::jobs::ExitStatus;

use super::Builtin;

#[derive(Parser)]
#[command(name = "alias", about = "Define or print aliases")]
pub struct AliasArgs {
    /// NAME=VALUE to define, or a bare NAME to print its current definition
    names: Vec<String>,
}

impl Builtin for AliasArgs {
    fn run(self, shell: &mut Shell) -> Result<ExitStatus> {
        if self.names.is_empty() {
            let mut pairs: Vec<(&str, &str)> = shell.env.all_aliases().collect();
            pairs.sort_by_key(|(k, _)| *k);
            let printed_any = !pairs.is_empty();
            for (k, v) in pairs {
                println!("alias {k}={}", shell_quote_always(v));
            }
            if printed_any {
                shell.note_stdout("\n");
            }
            return Ok(ExitStatus::SUCCESS);
        }
        for arg in &self.names {
            if let Some((k, v)) = arg.split_once('=') {
                shell.env.set_alias(k.to_owned(), v.to_owned());
            } else if let Some(v) = shell.env.get_alias(arg) {
                println!("alias {arg}={}", shell_quote_always(&v));
                shell.note_stdout("\n");
            } else {
                eprintln!("swagsh: alias: {arg}: not found");
                return Ok(ExitStatus::FAILURE);
            }
        }
        Ok(ExitStatus::SUCCESS)
    }
}

#[derive(Parser)]
#[command(name = "unalias", about = "Remove aliases")]
pub struct UnaliasArgs {
    /// Remove every alias
    #[arg(short = 'a')]
    all: bool,
    names: Vec<String>,
}

impl Builtin for UnaliasArgs {
    fn run(self, shell: &mut Shell) -> Result<ExitStatus> {
        if self.all {
            shell.env.clear_aliases();
            return Ok(ExitStatus::SUCCESS);
        }
        let mut status = ExitStatus::SUCCESS;
        for name in &self.names {
            if !shell.env.remove_alias(name) {
                eprintln!("swagsh: unalias: {name}: not found");
                status = ExitStatus::FAILURE;
            }
        }
        Ok(status)
    }
}
