use anyhow::{Result, anyhow};

use crate::eval::Shell;
use crate::expand::expand_tilde;
use crate::jobs::ExitStatus;

pub fn builtin_cd(shell: &mut Shell, args: &[&str]) -> Result<ExitStatus> {
    let target = match args.first() {
        Some(&"-") => shell.env.get("OLDPWD").unwrap_or_else(|| "/".into()),
        Some(&path) => expand_tilde(path, &shell.env),
        None => shell.env.get("HOME").unwrap_or_else(|| "/".into()),
    };
    let old = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    std::env::set_current_dir(&target).map_err(|e| anyhow!("cd: {target}: {e}"))?;
    shell.env.export("OLDPWD", old);
    let new = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    shell.env.export("PWD", new);
    Ok(ExitStatus::SUCCESS)
}

pub fn builtin_pwd(_shell: &mut Shell, _args: &[&str]) -> Result<ExitStatus> {
    println!(
        "{}",
        std::env::current_dir()
            .map_err(|e| anyhow!("pwd: {e}"))?
            .display()
    );
    Ok(ExitStatus::SUCCESS)
}
