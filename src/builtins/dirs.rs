use anyhow::{Result, anyhow};
use clap::Parser;

use crate::errfmt::strerror;
use crate::eval::Shell;
use crate::expand::expand_tilde;
use crate::jobs::ExitStatus;

use super::Builtin;

/// Collapses `.`/`..` components in `base/target` purely as text, without
/// touching the filesystem, so a directory reached through a symlink keeps
/// the traversed (not resolved) path in `$PWD` by default: the same
/// `-L`/`-P` distinction `cd`/`pwd` make.
fn normalize_logical(base: &str, target: &str) -> String {
    let joined = if target.starts_with('/') {
        target.to_owned()
    } else if base.is_empty() {
        format!("/{target}")
    } else {
        format!("{base}/{target}")
    };
    let mut stack: Vec<&str> = Vec::new();
    for comp in joined.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                stack.pop();
            }
            c => stack.push(c),
        }
    }
    format!("/{}", stack.join("/"))
}

#[derive(Parser)]
#[command(name = "cd", about = "Change the shell working directory")]
// Each bool is an independent clap-derived CLI flag, always constructed by
// clap from named flags rather than positionally.
#[allow(clippy::struct_excessive_bools)]
pub struct CdBuiltin {
    /// Resolve symlinks after processing `..` in DIR (default)
    #[arg(short = 'L', overrides_with = "physical")]
    logical: bool,
    /// Resolve symlinks before processing `..`, so $PWD never names a symlink
    #[arg(short = 'P', overrides_with = "logical")]
    physical: bool,
    /// With -P, exit non-zero if the new working directory can't be read back
    #[arg(short = 'e')]
    exit_on_fail: bool,
    /// Accepted for compatibility; extended-attribute directories aren't a Linux concept
    #[arg(short = '@')]
    xattr: bool,
    dir: Option<String>,
}

impl Builtin for CdBuiltin {
    fn run(self, shell: &mut Shell) -> Result<ExitStatus> {
        let _ = self.xattr;
        let physical = self.physical && !self.logical;

        let old_pwd = shell.env.get("PWD").unwrap_or_default();
        let (target, print_target) = match self.dir.as_deref() {
            Some("-") => (shell.env.get("OLDPWD").unwrap_or_else(|| "/".into()), true),
            Some(path) => (expand_tilde(path, &shell.env), false),
            None => (shell.env.get("HOME").unwrap_or_else(|| "/".into()), false),
        };

        let candidate = normalize_logical(&old_pwd, &target);
        std::env::set_current_dir(&candidate)
            .map_err(|e| anyhow!("cd: {target}: {}", strerror(e)))?;
        shell.env.export("OLDPWD", old_pwd);

        let new_pwd = if physical {
            match std::env::current_dir() {
                Ok(p) => p.to_string_lossy().into_owned(),
                Err(_) if self.exit_on_fail => return Ok(ExitStatus::FAILURE),
                Err(_) => candidate,
            }
        } else {
            candidate
        };
        shell.env.export("PWD", &new_pwd);
        if print_target {
            println!("{new_pwd}");
            shell.note_stdout("\n");
        }
        Ok(ExitStatus::SUCCESS)
    }
}

#[derive(Parser)]
#[command(name = "pwd", about = "Print the current working directory")]
pub struct PwdBuiltin {
    /// Print $PWD as-is, without resolving symlinks (default)
    #[arg(short = 'L', overrides_with = "physical")]
    logical: bool,
    /// Resolve all symlinks before printing
    #[arg(short = 'P', overrides_with = "logical")]
    physical: bool,
}

impl Builtin for PwdBuiltin {
    fn run(self, shell: &mut Shell) -> Result<ExitStatus> {
        let physical = self.physical && !self.logical;
        if !physical && let Some(pwd) = shell.env.get("PWD").filter(|p| !p.is_empty()) {
            println!("{pwd}");
            shell.note_stdout("\n");
            return Ok(ExitStatus::SUCCESS);
        }
        println!(
            "{}",
            std::env::current_dir()
                .map_err(|e| anyhow!("pwd: {}", strerror(e)))?
                .display()
        );
        shell.note_stdout("\n");
        Ok(ExitStatus::SUCCESS)
    }
}
