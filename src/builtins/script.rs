//! Dynamic-execution builtins: `eval` (parse and run a string in the
//! current shell), `exec` (replace the shell's own process image), and
//! `source`/`.` (run a file's commands in the current shell). Grouped
//! separately from `flow.rs`'s loop/function control-flow signals since
//! this is a different concern: each of these runs *new* code rather than
//! altering control flow through code already running.

use anyhow::Result;
use clap::Parser;

use crate::errfmt::strerror;
use crate::eval::Shell;
use crate::jobs::ExitStatus;

use super::Builtin;

/// `eval` has no real flags of its own (just `eval [arg ...]`), but still
/// goes through clap for the same reasons every other flagless-but-variadic
/// builtin does: uniform `--help` (confirmed `eval --help` should show this
/// exact text, not try to run `--help` as a command), and uniform
/// bad-argument handling. `allow_hyphen_values` +
/// `trailing_var_arg` keep a `-`-led word (`eval "echo -n hi"` has none,
/// but `eval -n foo` legitimately should try to run `-n foo` as a command)
/// from being misread as a flag of `eval`'s own.
#[derive(Parser)]
#[command(
    name = "eval",
    about = "Execute arguments as a shell command",
    trailing_var_arg = true
)]
pub struct EvalBuiltin {
    #[arg(allow_hyphen_values = true)]
    words: Vec<String>,
}

impl Builtin for EvalBuiltin {
    fn run(self, shell: &mut Shell) -> Result<ExitStatus> {
        if self.words.is_empty() {
            return Ok(ExitStatus::SUCCESS);
        }
        let src = self.words.join(" ");
        let program = crate::parser::parse(&src).map_err(|e| anyhow::anyhow!("eval: {e}"))?;
        shell.run_program(&program)
    }
}

/// `exec`'s flags, parsed via `cli::parse_args` directly rather than the
/// usual `Builtin`/`dispatch` path: a bare `exec` (no COMMAND) needs its
/// redirects applied *permanently* to the current shell rather than scoped
/// to a single call the way every other builtin's redirects are, which only
/// `Shell::run_exec_builtin` (in `eval/exec.rs`, where `apply_redirects`
/// lives) is positioned to do.
#[derive(Parser)]
#[command(
    name = "exec",
    about = "Replace the shell with the given command",
    trailing_var_arg = true
)]
pub struct ExecBuiltin {
    /// Pass NAME as the zeroth argument to COMMAND
    #[arg(short = 'a')]
    pub name: Option<String>,
    /// Execute COMMAND with an empty environment
    #[arg(short = 'c')]
    pub clear_env: bool,
    /// Place a dash in the zeroth argument to COMMAND
    #[arg(short = 'l')]
    pub login: bool,
    /// COMMAND followed by *its* argv, kept as one trailing field (rather
    /// than a separate `command: Option<String>` + `arguments: Vec<String>`)
    /// so `trailing_var_arg` actually disables flag-parsing for all of it:
    /// split across two positional fields, clap kept recognizing e.g. the
    /// `-c` in `exec -a x sh -c "..."` as exec's own `-c` instead of
    /// COMMAND's argument, even with `allow_hyphen_values` on both.
    #[arg(allow_hyphen_values = true, value_name = "COMMAND")]
    pub command_and_args: Vec<String>,
}

/// Placeholder for the `BUILTINS` table: `eval::Shell::run_builtin`
/// special-cases the name `"exec"` before ever calling the resolved
/// `BuiltinFn`, so this never actually runs (see `ExecBuiltin`'s doc comment).
pub fn builtin_exec_unreachable(_: &mut Shell, _: &[&str]) -> Result<ExitStatus> {
    unreachable!("exec is special-cased in Shell::run_builtin")
}

#[derive(Parser)]
#[command(
    name = "source",
    about = "Execute commands from a file in the current shell"
)]
pub struct SourceBuiltin {
    /// Search PATH (colon-separated) for FILENAME instead of $PATH
    #[arg(short = 'p')]
    path: Option<String>,
    filename: String,
    arguments: Vec<String>,
}

/// Resolves `filename` the conventional `source` way: used as-is if it
/// contains a `/` or exists relative to the current directory, otherwise
/// searched for as a bare name across `search_path` (`-p`'s argument, or
/// `$PATH` if `-p` wasn't given) the same way an external command is.
fn resolve_source_path(filename: &str, search_path: Option<&str>, shell: &Shell) -> String {
    if filename.contains('/') || std::path::Path::new(filename).exists() {
        return filename.to_owned();
    }
    let search_path = search_path
        .map(str::to_owned)
        .or_else(|| shell.env.get("PATH"))
        .unwrap_or_default();
    for dir in search_path.split(':') {
        let candidate = format!("{dir}/{filename}");
        if std::path::Path::new(&candidate).exists() {
            return candidate;
        }
    }
    filename.to_owned()
}

impl Builtin for SourceBuiltin {
    fn run(self, shell: &mut Shell) -> Result<ExitStatus> {
        let resolved = resolve_source_path(&self.filename, self.path.as_deref(), shell);
        let src = std::fs::read_to_string(&resolved)
            .map_err(|e| anyhow::anyhow!("source: {resolved}: {}", strerror(e)))?;
        let program =
            crate::parser::parse(&src).map_err(|e| anyhow::anyhow!("source: {resolved}: {e}"))?;

        // Extra arguments become $1, $2, ... only for this run, restored
        // afterward; with none given, the caller's own positional params
        // stay visible to the sourced script.
        let old_args = (!self.arguments.is_empty()).then(|| {
            let old = shell.env.positional_args().to_vec();
            shell.env.set_positional_args(self.arguments.clone());
            old
        });
        let result = shell.run_program(&program);
        if let Some(old) = old_args {
            shell.env.set_positional_args(old);
        }
        result
    }
}
