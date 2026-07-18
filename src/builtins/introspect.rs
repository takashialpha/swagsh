//! `type` and `command` share this file rather than each getting their own:
//! `command`'s default mode *runs* something (bypassing function lookup),
//! which on its face is a different concern from `type`'s pure
//! introspection; but `command -v`/`-V` really are introspection, and
//! both builtins need the exact same "is NAME a builtin, a function, or a
//! file on PATH" classification (`classify`/`Kind`/`find_in_path` below) to
//! answer it. Keeping them together avoids either duplicating that logic
//! or extracting it into a third file for two callers.

use anyhow::Result;
use clap::Parser;

use crate::env::Env;
use crate::errfmt::emit;
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
pub struct TypeBuiltin {
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

impl Builtin for TypeBuiltin {
    fn run(self, shell: &mut Shell) -> Result<ExitStatus> {
        let mut status = ExitStatus::SUCCESS;
        let mut printed_any = false;
        for name in &self.names {
            let mut kinds = classify(name, &shell.env);
            if kinds.is_empty() {
                if !self.path_only && !self.type_only {
                    emit(format!("type: {name}: not found"));
                }
                status = ExitStatus::FAILURE;
                continue;
            }
            if !self.all {
                kinds.truncate(1);
            }
            for kind in kinds {
                match kind {
                    Kind::Builtin | Kind::Function if self.path_only => {}
                    Kind::Builtin if self.type_only => {
                        println!("builtin");
                        printed_any = true;
                    }
                    Kind::Builtin => {
                        println!("{name} is a shell builtin");
                        printed_any = true;
                    }
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

/// A guaranteed-to-find-the-standard-utilities `PATH`, used by `command -p`
/// instead of the shell's own possibly-tampered-with `$PATH`.
const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

#[derive(Parser)]
#[command(
    name = "command",
    about = "Run COMMAND, bypassing shell function lookup",
    trailing_var_arg = true
)]
pub struct CommandBuiltin {
    /// Search a default PATH guaranteed to find the standard utilities
    #[arg(short = 'p')]
    default_path: bool,
    /// Print the word that would invoke COMMAND, instead of running it
    #[arg(short = 'v')]
    print_word: bool,
    /// Print a fuller description of what would invoke COMMAND
    #[arg(short = 'V')]
    print_verbose: bool,
    /// COMMAND followed by its own argv, kept as one trailing field (like
    /// `exec`'s `command_and_args`) so a flag-shaped argument to COMMAND
    /// itself (`command grep -v x`) isn't misparsed as belonging to
    /// `command`.
    #[arg(allow_hyphen_values = true, value_name = "COMMAND")]
    command_and_args: Vec<String>,
}

impl Builtin for CommandBuiltin {
    fn run(self, shell: &mut Shell) -> Result<ExitStatus> {
        let Some((name, rest)) = self.command_and_args.split_first() else {
            return Ok(ExitStatus::SUCCESS);
        };

        if self.print_word || self.print_verbose {
            return Ok(describe_command(shell, name, self.print_verbose));
        }

        // Bypassing function lookup is the entire point of `command`: a
        // builtin still runs (`command cd` is still the `cd` builtin), but
        // a same-named function is skipped in favor of PATH/builtin
        // resolution, unlike plain `cd`.
        if let Some(f) = crate::builtins::lookup_builtin(name) {
            let arg_refs: Vec<&str> = rest.iter().map(String::as_str).collect();
            return f(shell, &arg_refs);
        }

        let old_path = self.default_path.then(|| {
            let old = shell.env.get("PATH");
            shell.env.set("PATH", DEFAULT_PATH);
            old
        });
        let empty_sc = crate::ast::SimpleCmd {
            words: Vec::new(),
            redirects: Vec::new(),
        };
        let result = shell.run_external(&empty_sc, &self.command_and_args, &[]);
        if let Some(old) = old_path {
            match old {
                Some(p) => shell.env.set("PATH", p),
                None => shell.env.unset_var("PATH"),
            }
        }
        result
    }
}

/// `command -v`/`-V`: unlike `type`, only the first match is ever reported
/// (no `-a`), and `-v`'s output is a bare word rather than `type`'s "NAME
/// is ..." sentence.
fn describe_command(shell: &mut Shell, name: &str, verbose: bool) -> ExitStatus {
    let Some(kind) = classify(name, &shell.env).into_iter().next() else {
        if verbose {
            emit(format!("command: {name}: not found"));
        }
        return ExitStatus::FAILURE;
    };
    if verbose {
        match kind {
            Kind::Builtin => println!("{name} is a shell builtin"),
            Kind::Function => println!("{name} is a function"),
            Kind::File(path) => println!("{name} is {path}"),
        }
    } else {
        match kind {
            Kind::Builtin | Kind::Function => println!("{name}"),
            Kind::File(path) => println!("{path}"),
        }
    }
    shell.note_stdout("\n");
    ExitStatus::SUCCESS
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
