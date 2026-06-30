mod alias;
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

// Sorted table — binary search in lookup_builtin requires strict lexicographic order.
pub static BUILTINS: &[(&str, BuiltinFn)] = &[
    (".", flow::builtin_source),
    (":", flow::builtin_colon),
    ("[", test::builtin_bracket),
    ("alias", alias::builtin_alias),
    ("bg", jobs::builtin_bg),
    ("break", flow::builtin_break),
    ("cd", dirs::builtin_cd),
    ("continue", flow::builtin_continue),
    ("echo", output::builtin_echo),
    ("exec", flow::builtin_exec),
    ("exit", flow::builtin_exit),
    ("export", env::builtin_export),
    ("false", flow::builtin_false),
    ("fg", jobs::builtin_fg),
    ("jobs", jobs::builtin_jobs),
    ("kill", jobs::builtin_kill),
    ("printf", output::builtin_printf),
    ("pwd", dirs::builtin_pwd),
    ("read", io::builtin_read),
    ("return", flow::builtin_return),
    ("set", env::builtin_set),
    ("shift", env::builtin_shift),
    ("source", flow::builtin_source),
    ("test", test::builtin_test),
    ("true", flow::builtin_true),
    ("type", introspect::builtin_type),
    ("unalias", alias::builtin_unalias),
    ("unset", env::builtin_unset),
];

#[inline]
pub fn lookup_builtin(name: &str) -> Option<BuiltinFn> {
    BUILTINS
        .binary_search_by_key(&name, |&(k, _)| k)
        .ok()
        .map(|i| BUILTINS[i].1)
}
