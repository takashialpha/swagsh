mod alias;
pub mod cli;
pub mod declare;
mod dirs;
mod echo;
mod env;
pub mod flow;
mod getopts;
pub mod help;
pub mod history;
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
pub use env::restore_options;
use help::{HelpPage, hand_parsed, page};

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

    /// Examples for its `help` page, beyond what clap describes.
    #[must_use]
    fn examples() -> &'static [help::Example] {
        &[]
    }

    /// Notes for its `help` page.
    #[must_use]
    fn notes() -> &'static [&'static str] {
        &[]
    }
}

/// A builtin's help page, for the builtins table.
pub type HelpFn = fn() -> HelpPage;

// Sorted table: binary search in lookup_builtin requires strict lexicographic order.
// Each entry is a builtin's name, how to run it, and its help page. A builtin
// is either a `Builtin` flag struct, run through `dispatch`, or a
// `HandParsed` type for one whose argument grammar isn't clap-shaped (each
// such case is documented at its definition), run through `hand_parsed`.
pub static BUILTINS: &[(&str, BuiltinFn, HelpFn)] = &[
    (
        ".",
        dispatch::<script::SourceBuiltin>,
        page::<script::SourceBuiltin>,
    ),
    (":", hand_parsed::<flow::Colon>, page::<flow::Colon>),
    ("[", hand_parsed::<test::Bracket>, page::<test::Bracket>),
    (
        "alias",
        dispatch::<alias::AliasBuiltin>,
        page::<alias::AliasBuiltin>,
    ),
    ("bg", dispatch::<jobs::BgBuiltin>, page::<jobs::BgBuiltin>),
    (
        "break",
        dispatch::<flow::BreakBuiltin>,
        page::<flow::BreakBuiltin>,
    ),
    (
        "builtin",
        hand_parsed::<flow::BuiltinCmd>,
        page::<flow::BuiltinCmd>,
    ),
    ("cd", dispatch::<dirs::CdBuiltin>, page::<dirs::CdBuiltin>),
    (
        "command",
        dispatch::<introspect::CommandBuiltin>,
        page::<introspect::CommandBuiltin>,
    ),
    (
        "continue",
        dispatch::<flow::ContinueBuiltin>,
        page::<flow::ContinueBuiltin>,
    ),
    (
        "declare",
        hand_parsed::<declare::Declare>,
        page::<declare::Declare>,
    ),
    (
        "dirs",
        dispatch::<dirs::DirsBuiltin>,
        page::<dirs::DirsBuiltin>,
    ),
    (
        "disown",
        dispatch::<jobs::DisownBuiltin>,
        page::<jobs::DisownBuiltin>,
    ),
    ("echo", hand_parsed::<echo::Echo>, page::<echo::Echo>),
    (
        "eval",
        dispatch::<script::EvalBuiltin>,
        page::<script::EvalBuiltin>,
    ),
    (
        "exec",
        script::builtin_exec_unreachable,
        page::<script::Exec>,
    ),
    (
        "exit",
        dispatch::<flow::ExitBuiltin>,
        page::<flow::ExitBuiltin>,
    ),
    (
        "export",
        hand_parsed::<declare::Export>,
        page::<declare::Export>,
    ),
    ("false", hand_parsed::<flow::False>, page::<flow::False>),
    (
        "fc",
        dispatch::<history::FcBuiltin>,
        page::<history::FcBuiltin>,
    ),
    ("fg", dispatch::<jobs::FgBuiltin>, page::<jobs::FgBuiltin>),
    (
        "getopts",
        dispatch::<getopts::GetoptsBuiltin>,
        page::<getopts::GetoptsBuiltin>,
    ),
    (
        "hash",
        dispatch::<introspect::HashBuiltin>,
        page::<introspect::HashBuiltin>,
    ),
    (
        "help",
        dispatch::<help::HelpBuiltin>,
        page::<help::HelpBuiltin>,
    ),
    (
        "history",
        dispatch::<history::HistoryBuiltin>,
        page::<history::HistoryBuiltin>,
    ),
    (
        "jobs",
        dispatch::<jobs::JobsBuiltin>,
        page::<jobs::JobsBuiltin>,
    ),
    (
        "kill",
        dispatch::<kill::KillBuiltin>,
        page::<kill::KillBuiltin>,
    ),
    ("let", hand_parsed::<flow::Let>, page::<flow::Let>),
    (
        "local",
        hand_parsed::<declare::Local>,
        page::<declare::Local>,
    ),
    (
        "mapfile",
        dispatch::<declare::MapfileBuiltin>,
        page::<declare::MapfileBuiltin>,
    ),
    (
        "popd",
        dispatch::<dirs::PopdBuiltin>,
        page::<dirs::PopdBuiltin>,
    ),
    (
        "printf",
        dispatch::<printf::PrintfBuiltin>,
        page::<printf::PrintfBuiltin>,
    ),
    (
        "pushd",
        dispatch::<dirs::PushdBuiltin>,
        page::<dirs::PushdBuiltin>,
    ),
    (
        "pwd",
        dispatch::<dirs::PwdBuiltin>,
        page::<dirs::PwdBuiltin>,
    ),
    (
        "read",
        dispatch::<read::ReadBuiltin>,
        page::<read::ReadBuiltin>,
    ),
    (
        "readarray",
        dispatch::<declare::MapfileBuiltin>,
        page::<declare::MapfileBuiltin>,
    ),
    (
        "readonly",
        hand_parsed::<declare::Readonly>,
        page::<declare::Readonly>,
    ),
    (
        "return",
        dispatch::<flow::ReturnBuiltin>,
        page::<flow::ReturnBuiltin>,
    ),
    ("set", hand_parsed::<env::Set>, page::<env::Set>),
    (
        "shift",
        dispatch::<env::ShiftBuiltin>,
        page::<env::ShiftBuiltin>,
    ),
    (
        "shopt",
        hand_parsed::<shopt::ShoptCmd>,
        page::<shopt::ShoptCmd>,
    ),
    (
        "source",
        dispatch::<script::SourceBuiltin>,
        page::<script::SourceBuiltin>,
    ),
    ("test", hand_parsed::<test::Test>, page::<test::Test>),
    (
        "times",
        dispatch::<limits::TimesBuiltin>,
        page::<limits::TimesBuiltin>,
    ),
    ("trap", hand_parsed::<trap::Trap>, page::<trap::Trap>),
    ("true", hand_parsed::<flow::True>, page::<flow::True>),
    (
        "type",
        dispatch::<introspect::TypeBuiltin>,
        page::<introspect::TypeBuiltin>,
    ),
    (
        "typeset",
        hand_parsed::<declare::Declare>,
        page::<declare::Declare>,
    ),
    (
        "ulimit",
        hand_parsed::<limits::Ulimit>,
        page::<limits::Ulimit>,
    ),
    (
        "umask",
        dispatch::<limits::UmaskBuiltin>,
        page::<limits::UmaskBuiltin>,
    ),
    (
        "unalias",
        dispatch::<alias::UnaliasBuiltin>,
        page::<alias::UnaliasBuiltin>,
    ),
    (
        "unset",
        dispatch::<env::UnsetBuiltin>,
        page::<env::UnsetBuiltin>,
    ),
    (
        "wait",
        dispatch::<wait::WaitBuiltin>,
        page::<wait::WaitBuiltin>,
    ),
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
        .binary_search_by_key(&name, |&(k, ..)| k)
        .ok()
        .map(|i| BUILTINS[i].1)
}
