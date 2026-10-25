use anyhow::{Result, anyhow};
use clap::Parser as _;

use swagsh::cli::Cli;
use swagsh::env::Env;
use swagsh::errfmt::{emit, strerror};
use swagsh::eval::{Shell, SyntaxError};
use swagsh::{parser, repl, signal};

fn main() -> Result<()> {
    signal::reset_sigpipe();
    let argv0 = std::env::args().next().unwrap_or_default();
    let cli = Cli::parse();

    let (script, mut args) = cli.split_positionals();
    let dollar_zero = match (&cli.command, &script) {
        (Some(_), _) if !args.is_empty() => Some(args.remove(0)),
        (None, Some(path)) => Some(path.display().to_string()),
        _ => None,
    };

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

    // For scripts to tell they run in swagsh; not exported.
    env.set("SWAGSH_VERSION", env!("CARGO_PKG_VERSION"));

    if env.get("SHELL").is_none_or(|s| s.is_empty())
        && let Ok(exe) = std::env::current_exe()
    {
        env.export("SHELL", exe.to_string_lossy());
    }

    if let Some(cmd) = &cli.command {
        let mut shell = Shell::new(env, false);
        shell.arg0 = dollar_zero.unwrap_or(shell.arg0);
        shell.command_string = Some(cmd.clone());
        return run_and_exit(&mut shell, cmd, &cli);
    }

    if let Some(path) = &script {
        let mut shell = Shell::new(env, false);
        shell.arg0 = dollar_zero.unwrap_or(shell.arg0);
        shell.script = true;
        shell.sources.push(path.to_string_lossy().into());
        let src = std::fs::read_to_string(path)
            .map_err(|e| anyhow!("{}: {}", path.display(), strerror(e)))?;
        return run_and_exit(&mut shell, &src, &cli);
    }

    let is_login = cli.login || Cli::is_login_shell(&argv0);
    let mut shell = Shell::new(env, true);
    shell.login = is_login;
    if !cli.no_rc {
        repl::source_startup_files(&mut shell, is_login);
    }
    repl::run_interactive(shell, &cli)?;
    Ok(())
}

/// Parses and (unless `--dry-run`) runs `src`, exiting with the program's
/// status. Parse and execution errors go through `errfmt::emit`, the same
/// path the interactive REPL uses, instead of being left to propagate via
/// `?` to `main`'s default `Result` handler: that would print them as
/// `Error: <Debug>` (a different prefix than the rest of swagsh's output,
/// and without `emit`'s `(os error N)` stripping). Errors that don't fork
/// (a builtin's or a shell function's own errors, since `run_builtin` and
/// `run_function` run in this same process) take exactly this path, so
/// skipping it here left them unformatted for `-c` and script invocations.
fn run_and_exit(shell: &mut Shell, src: &str, cli: &Cli) -> Result<()> {
    if cli.no_execute {
        if let Err(e) = parser::parse(src) {
            emit(e);
            std::process::exit(2);
        }
        return Ok(());
    }
    match shell.run_source(src, "") {
        Ok(status) => shell.exit(status.0),
        Err(e) => {
            emit(&e);
            shell.exit(if e.is::<SyntaxError>() { 2 } else { 1 });
        }
    }
}
