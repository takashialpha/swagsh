mod alias;
pub mod cli;
pub mod declare;
mod dirs;
mod echo;
mod env;
pub mod flow;
mod getopts;
mod history;
mod introspect;
mod jobs;
mod kill;
pub mod limits;
mod printf;
mod read;
pub mod script;
mod shopt;
pub mod test;
mod trap;
mod wait;

use crate::eval::Shell;
use crate::jobs::ExitStatus;

use cli::dispatch;

pub type BuiltinFn = fn(&mut Shell, &[&str]) -> anyhow::Result<ExitStatus>;

/// A builtin failed and has already said why: the caller only needs the
/// status. Returned as an error rather than a plain status so that a
/// special builtin's failure can end a non-interactive shell, as POSIX
/// requires.
#[derive(Debug)]
pub struct BuiltinFailed(pub ExitStatus);

impl std::fmt::Display for BuiltinFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "failed with status {}", self.0.0)
    }
}

impl std::error::Error for BuiltinFailed {}

/// The alternative to [`BuiltinFn`] for a builtin whose flags are worth
/// real parsing.
///
/// A `clap`-derived struct that carries its own name, flags,
/// and help text (via `#[command(name = "...")]`/`#[arg(...)]`) and knows
/// how to run itself. `dispatch::<T>` adapts any `T: Builtin` back into a
/// plain `BuiltinFn`, so the struct is the entire builtin: no separate
/// wrapper function to keep in sync with it.
pub trait Builtin: clap::Parser {
    /// # Errors
    ///
    /// Returns an error if the builtin fails.
    fn run(self, shell: &mut Shell) -> anyhow::Result<ExitStatus>;
}

// Sorted table: binary search in lookup_builtin requires strict lexicographic order.
// Entries are either a hand-written `BuiltinFn` (builtins like `echo`/`test`
// whose argument grammar isn't clap-shaped, each such case is documented at
// its definition) or `dispatch::<XBuiltin>` for a `Builtin` flag struct.
pub static BUILTINS: &[(&str, BuiltinFn)] = &[
    (".", dispatch::<script::SourceBuiltin>),
    (":", flow::builtin_colon),
    ("[", test::builtin_bracket),
    ("alias", dispatch::<alias::AliasBuiltin>),
    ("bg", dispatch::<jobs::BgBuiltin>),
    ("break", dispatch::<flow::BreakBuiltin>),
    ("builtin", flow::builtin_builtin),
    ("cd", dispatch::<dirs::CdBuiltin>),
    ("command", dispatch::<introspect::CommandBuiltin>),
    ("continue", dispatch::<flow::ContinueBuiltin>),
    ("declare", declare::builtin_declare),
    ("dirs", dispatch::<dirs::DirsBuiltin>),
    ("disown", dispatch::<jobs::DisownBuiltin>),
    ("echo", echo::builtin_echo),
    ("eval", dispatch::<script::EvalBuiltin>),
    ("exec", script::builtin_exec_unreachable),
    ("exit", dispatch::<flow::ExitBuiltin>),
    ("export", declare::builtin_export),
    ("false", flow::builtin_false),
    ("fc", dispatch::<history::FcBuiltin>),
    ("fg", dispatch::<jobs::FgBuiltin>),
    ("getopts", dispatch::<getopts::GetoptsBuiltin>),
    ("hash", dispatch::<introspect::HashBuiltin>),
    ("history", dispatch::<history::HistoryBuiltin>),
    ("jobs", dispatch::<jobs::JobsBuiltin>),
    ("kill", dispatch::<kill::KillBuiltin>),
    ("let", flow::builtin_let),
    ("local", declare::builtin_local),
    ("mapfile", dispatch::<declare::MapfileBuiltin>),
    ("popd", dispatch::<dirs::PopdBuiltin>),
    ("printf", dispatch::<printf::PrintfBuiltin>),
    ("pushd", dispatch::<dirs::PushdBuiltin>),
    ("pwd", dispatch::<dirs::PwdBuiltin>),
    ("read", dispatch::<read::ReadBuiltin>),
    ("readarray", dispatch::<declare::MapfileBuiltin>),
    ("readonly", declare::builtin_readonly),
    ("return", dispatch::<flow::ReturnBuiltin>),
    ("set", env::builtin_set),
    ("shift", dispatch::<env::ShiftBuiltin>),
    ("shopt", shopt::builtin_shopt),
    ("source", dispatch::<script::SourceBuiltin>),
    ("test", test::builtin_test),
    ("times", dispatch::<limits::TimesBuiltin>),
    ("trap", trap::builtin_trap),
    ("true", flow::builtin_true),
    ("type", dispatch::<introspect::TypeBuiltin>),
    ("typeset", declare::builtin_declare),
    ("ulimit", limits::builtin_ulimit),
    ("umask", dispatch::<limits::UmaskBuiltin>),
    ("unalias", dispatch::<alias::UnaliasBuiltin>),
    ("unset", dispatch::<env::UnsetBuiltin>),
    ("wait", dispatch::<wait::WaitBuiltin>),
];

/// Reads one line from descriptor `fd`, as `read -r` does, without the
/// newline; and whether the line ended in one rather than at end of input.
///
/// # Errors
///
/// Returns an error if the read fails.
pub fn read_line(fd: i32) -> std::io::Result<(Vec<u8>, bool)> {
    read::read_until_delim(fd, b'\n', true)
}

/// The `shopt` options that are on, for `BASHOPTS`.
#[must_use]
pub fn shopt_on(shell: &Shell) -> Vec<&'static str> {
    shopt::on(shell)
}

#[inline]
pub fn lookup_builtin(name: &str) -> Option<BuiltinFn> {
    BUILTINS
        .binary_search_by_key(&name, |&(k, _)| k)
        .ok()
        .map(|i| BUILTINS[i].1)
}
