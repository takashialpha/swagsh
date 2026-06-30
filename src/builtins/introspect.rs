use anyhow::Result;

use crate::env::Env;
use crate::eval::Shell;
use crate::jobs::ExitStatus;

#[allow(clippy::unnecessary_wraps)] // required by BuiltinFn signature
pub fn builtin_type(shell: &mut Shell, args: &[&str]) -> Result<ExitStatus> {
    let mut status = ExitStatus::SUCCESS;
    for name in args {
        if crate::builtins::lookup_builtin(name).is_some() {
            println!("{name} is a shell builtin");
        } else if shell.env.get_function(name).is_some() {
            println!("{name} is a function");
        } else if let Some(path) = find_in_path(name, &shell.env) {
            println!("{name} is {path}");
        } else {
            eprintln!("swagsh: type: {name}: not found");
            status = ExitStatus::FAILURE;
        }
    }
    Ok(status)
}

fn find_in_path(name: &str, env: &Env) -> Option<String> {
    let path_var = env.get("PATH").unwrap_or_default();
    for dir in path_var.split(':') {
        let full = format!("{dir}/{name}");
        if std::fs::metadata(&full).is_ok() {
            return Some(full);
        }
    }
    None
}
