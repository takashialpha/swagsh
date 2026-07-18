mod alias;
pub mod cli;
mod dirs;
mod echo;
mod env;
pub mod flow;
mod getopts;
mod introspect;
mod jobs;
mod kill;
mod printf;
mod read;
pub mod script;
mod test;
mod wait;

use crate::eval::Shell;
use crate::jobs::ExitStatus;

use cli::dispatch;

pub type BuiltinFn = fn(&mut Shell, &[&str]) -> anyhow::Result<ExitStatus>;

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
    ("cd", dispatch::<dirs::CdBuiltin>),
    ("command", dispatch::<introspect::CommandBuiltin>),
    ("continue", dispatch::<flow::ContinueBuiltin>),
    ("echo", echo::builtin_echo),
    ("eval", dispatch::<script::EvalBuiltin>),
    ("exec", script::builtin_exec_unreachable),
    ("exit", dispatch::<flow::ExitBuiltin>),
    ("export", dispatch::<env::ExportBuiltin>),
    ("false", flow::builtin_false),
    ("fg", dispatch::<jobs::FgBuiltin>),
    ("getopts", dispatch::<getopts::GetoptsBuiltin>),
    ("jobs", dispatch::<jobs::JobsBuiltin>),
    ("kill", dispatch::<kill::KillBuiltin>),
    ("printf", dispatch::<printf::PrintfBuiltin>),
    ("pwd", dispatch::<dirs::PwdBuiltin>),
    ("read", dispatch::<read::ReadBuiltin>),
    ("return", dispatch::<flow::ReturnBuiltin>),
    ("set", env::builtin_set),
    ("shift", dispatch::<env::ShiftBuiltin>),
    ("source", dispatch::<script::SourceBuiltin>),
    ("test", test::builtin_test),
    ("true", flow::builtin_true),
    ("type", dispatch::<introspect::TypeBuiltin>),
    ("unalias", dispatch::<alias::UnaliasBuiltin>),
    ("unset", dispatch::<env::UnsetBuiltin>),
    ("wait", dispatch::<wait::WaitBuiltin>),
];

#[inline]
pub fn lookup_builtin(name: &str) -> Option<BuiltinFn> {
    BUILTINS
        .binary_search_by_key(&name, |&(k, _)| k)
        .ok()
        .map(|i| BUILTINS[i].1)
}
