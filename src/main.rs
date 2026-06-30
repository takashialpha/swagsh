mod ast;
mod builtins;
mod cli;
mod env;
mod eval;
mod expand;
mod fd;
mod jobs;
mod lexer;
mod parser;
mod prompt;
mod repl;
mod signal;

use anyhow::{Result, anyhow};
use clap::Parser as _;

use cli::Cli;
use env::Env;
use eval::Shell;

fn main() -> Result<()> {
    let argv0 = std::env::args().next().unwrap_or_default();
    let cli = Cli::parse();

    let (script, args) = cli.split_positionals();

    let mut env = Env::from_process();
    if !args.is_empty() {
        env.set_positional_args(args);
    }

    if env.get("HOSTNAME").is_none_or(|h| h.is_empty())
        && let Ok(name) = std::fs::read_to_string("/etc/hostname")
    {
        let name = name.trim().to_owned();
        if !name.is_empty() {
            env.set("HOSTNAME", name);
        }
    }

    if let Some(cmd) = &cli.command {
        let mut shell = Shell::new(env, false);
        let program = parser::parse(cmd)?;
        if !cli.no_execute {
            let status = shell.run_program(&program)?;
            std::process::exit(status.0);
        }
        return Ok(());
    }

    if let Some(path) = &script {
        let mut shell = Shell::new(env, false);
        let src = std::fs::read_to_string(path).map_err(|e| anyhow!("{}: {e}", path.display()))?;
        let program = parser::parse(&src)?;
        if !cli.no_execute {
            let status = shell.run_program(&program)?;
            std::process::exit(status.0);
        }
        return Ok(());
    }

    let is_login = cli.login || Cli::is_login_shell(&argv0);
    let mut shell = Shell::new(env, true);
    if !cli.no_rc {
        repl::source_startup_files(&mut shell, is_login);
    }
    repl::run_interactive(shell, &cli)?;
    Ok(())
}
