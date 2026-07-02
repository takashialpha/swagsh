mod alias;
mod cli;
mod dirs;
mod env;
mod flow;
mod introspect;
mod io;
mod jobs;
mod output;
mod test;

use crate::eval::Shell;
use crate::jobs::ExitStatus;

pub type BuiltinFn = fn(&mut Shell, &[&str]) -> anyhow::Result<ExitStatus>;

/// The alternative to [`BuiltinFn`] for a builtin whose flags are worth
/// real parsing: a `clap`-derived struct that carries its own name, flags,
/// and help text (via `#[command(name = "...")]`/`#[arg(...)]`) and knows
/// how to run itself. `cli::dispatch::<T>` adapts any `T: Builtin` back
/// into a plain `BuiltinFn`, so the struct is the entire builtin: no
/// separate wrapper function to keep in sync with it.
pub trait Builtin: clap::Parser {
    fn run(self, shell: &mut Shell) -> anyhow::Result<ExitStatus>;
}

// Sorted table: binary search in lookup_builtin requires strict lexicographic order.
// Entries are either a hand-written `BuiltinFn` (builtins like `echo`/`test`
// whose argument grammar isn't clap-shaped) or `cli::dispatch::<XArgs>` for
// a `Builtin` flag struct.
pub static BUILTINS: &[(&str, BuiltinFn)] = &[
    (".", cli::dispatch::<flow::SourceArgs>),
    (":", flow::builtin_colon),
    ("[", test::builtin_bracket),
    ("alias", cli::dispatch::<alias::AliasArgs>),
    ("bg", jobs::builtin_bg),
    ("break", flow::builtin_break),
    ("cd", cli::dispatch::<dirs::CdArgs>),
    ("continue", flow::builtin_continue),
    ("echo", output::builtin_echo),
    ("exec", flow::builtin_exec),
    ("exit", flow::builtin_exit),
    ("export", cli::dispatch::<env::ExportArgs>),
    ("false", flow::builtin_false),
    ("fg", jobs::builtin_fg),
    ("jobs", cli::dispatch::<jobs::JobsArgs>),
    ("kill", jobs::builtin_kill),
    ("printf", output::builtin_printf),
    ("pwd", cli::dispatch::<dirs::PwdArgs>),
    ("read", cli::dispatch::<io::ReadArgs>),
    ("return", flow::builtin_return),
    ("set", env::builtin_set),
    ("shift", env::builtin_shift),
    ("source", cli::dispatch::<flow::SourceArgs>),
    ("test", test::builtin_test),
    ("true", flow::builtin_true),
    ("type", cli::dispatch::<introspect::TypeArgs>),
    ("unalias", cli::dispatch::<alias::UnaliasArgs>),
    ("unset", cli::dispatch::<env::UnsetArgs>),
];

#[inline]
pub fn lookup_builtin(name: &str) -> Option<BuiltinFn> {
    BUILTINS
        .binary_search_by_key(&name, |&(k, _)| k)
        .ok()
        .map(|i| BUILTINS[i].1)
}
