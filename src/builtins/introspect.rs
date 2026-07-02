use anyhow::Result;
use clap::Parser;

use crate::env::Env;
use crate::eval::Shell;
use crate::jobs::ExitStatus;

use super::Builtin;

enum Kind {
    Builtin,
    Function,
    File(String),
}

fn classify(name: &str, env: &Env) -> Vec<Kind> {
    let mut kinds = Vec::new();
    if crate::builtins::lookup_builtin(name).is_some() {
        kinds.push(Kind::Builtin);
    }
    if env.get_function(name).is_some() {
        kinds.push(Kind::Function);
    }
    if let Some(path) = find_in_path(name, env) {
        kinds.push(Kind::File(path));
    }
    kinds
}

#[derive(Parser)]
#[command(name = "type", about = "Describe how each NAME would be resolved")]
pub struct TypeArgs {
    /// Print only the type: "builtin", "function", "file" or nothing
    #[arg(short = 't')]
    type_only: bool,
    /// Print only the file path a NAME would exec to (silent otherwise)
    #[arg(short = 'p')]
    path_only: bool,
    /// List every match for NAME, not just the first
    #[arg(short = 'a')]
    all: bool,
    names: Vec<String>,
}

impl Builtin for TypeArgs {
    fn run(self, shell: &mut Shell) -> Result<ExitStatus> {
        let mut status = ExitStatus::SUCCESS;
        let mut printed_any = false;
        for name in &self.names {
            let mut kinds = classify(name, &shell.env);
            if kinds.is_empty() {
                if !self.path_only && !self.type_only {
                    eprintln!("swagsh: type: {name}: not found");
                }
                status = ExitStatus::FAILURE;
                continue;
            }
            if !self.all {
                kinds.truncate(1);
            }
            for kind in kinds {
                match kind {
                    Kind::Builtin if self.path_only => {}
                    Kind::Builtin if self.type_only => {
                        println!("builtin");
                        printed_any = true;
                    }
                    Kind::Builtin => {
                        println!("{name} is a shell builtin");
                        printed_any = true;
                    }
                    Kind::Function if self.path_only => {}
                    Kind::Function if self.type_only => {
                        println!("function");
                        printed_any = true;
                    }
                    Kind::Function => {
                        println!("{name} is a function");
                        printed_any = true;
                    }
                    Kind::File(path) if self.type_only => {
                        let _ = path;
                        println!("file");
                        printed_any = true;
                    }
                    Kind::File(path) if self.path_only => {
                        println!("{path}");
                        printed_any = true;
                    }
                    Kind::File(path) => {
                        println!("{name} is {path}");
                        printed_any = true;
                    }
                }
            }
        }
        if printed_any {
            shell.note_stdout("\n");
        }
        Ok(status)
    }
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
