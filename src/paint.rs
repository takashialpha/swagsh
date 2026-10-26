//! Color in swagsh's own output (errors, job states, listings), only where
//! it goes to a terminal and `$NO_COLOR` is unset.

use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::eval::Shell;

pub const ERROR: &str = "1;31";
pub const GOOD: &str = "32";
pub const WARN: &str = "33";
pub const DIM: &str = "2";

/// `$NO_COLOR` as the interactive shell last saw it among its variables,
/// for output that has no shell at hand. Scripts go by the environment
/// they were started with.
static NO_COLOR: AtomicBool = AtomicBool::new(false);

/// Records whether the shell has `$NO_COLOR` set.
pub fn set_no_color(set: bool) {
    NO_COLOR.store(set, Ordering::Relaxed);
}

fn no_color() -> bool {
    NO_COLOR.load(Ordering::Relaxed) || std::env::var_os("NO_COLOR").is_some()
}

/// Whether standard error should be in color.
#[must_use]
pub fn stderr() -> bool {
    !no_color() && rustix::termios::isatty(std::io::stderr())
}

/// Whether standard output should be in color, as `shell` sees it.
#[must_use]
pub fn stdout(shell: &Shell) -> bool {
    shell.env.get("NO_COLOR").is_none() && rustix::termios::isatty(std::io::stdout())
}

/// `text` in SGR style `sgr` when `on`.
#[must_use]
pub fn paint<'a>(text: &'a str, sgr: &str, on: bool) -> Cow<'a, str> {
    if on {
        Cow::Owned(format!("\x1b[{sgr}m{text}\x1b[0m"))
    } else {
        Cow::Borrowed(text)
    }
}
