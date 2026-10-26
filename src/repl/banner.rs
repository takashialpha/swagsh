//! The greeting an interactive shell starts with: a logo, the version and
//! a tip.

use std::fmt::Write as _;

use super::config::Banner;

/// The logo from the README.
const LOGO: [&str; 8] = [
    "                         ▗▖   ",
    "                         ▐▌   ",
    "▗▟██▖█   █ ▟██▖ ▟█▟▌▗▟██▖▐▙██▖",
    "▐▙▄▖▘▜ █ ▛ ▘▄▟▌▐▛ ▜▌▐▙▄▖▘▐▛ ▐▌",
    " ▀▀█▖▐▙█▟▌▗█▀▜▌▐▌ ▐▌ ▀▀█▖▐▌ ▐▌",
    "▐▄▄▟▌▝█ █▘▐▙▄█▌▝█▄█▌▐▄▄▟▌▐▌ ▐▌",
    " ▀▀▀  ▀ ▀  ▀▀▝▘ ▞▀▐▌ ▀▀▀ ▝▘ ▝▘",
    "                ▜█▛▘          ",
];

/// The row of the logo the version goes beside, with the tip under it.
const TEXT_ROW: usize = 3;

const TIPS: &[&str] = &[
    "`help` lists every builtin, and `help NAME` shows one",
    "press → to take the grey suggestion, from your history or a completion",
    "Tab completes commands, files, variables and builtin options",
    "`cd -` goes back to the previous directory",
    "`shopt -s autocd` lets you type a directory's name to go there",
    "`!!` repeats the last command, and `!$` is its last word",
    "`^old^new` runs the last command again with old replaced by new",
    "Ctrl-R searches your history",
    "`type NAME` tells what a command name runs",
    "`declare -p NAME` shows how a variable is set",
    "`shopt -s globstar` makes ** match any depth of directories",
    "`pushd` and `popd` keep a stack of directories",
    "`local -` in a function keeps its `set` changes to itself",
    "`printf '%(%H:%M)T\\n'` prints the time",
    "colors, this banner and more are set in ~/.config/swagsh/config.toml",
    "`set -o vi` gives vi keys, as does `prompt.keys = \"vi\"` in the config file",
    "Ctrl-X Ctrl-E edits the line in your editor, then runs it",
    "`prompt.format` in the config file sets the prompt, with `\\g` for git",
];

/// The banner, or nothing when it is turned off.
#[must_use]
pub fn render(banner: &Banner, color: bool) -> String {
    if !banner.enabled {
        return String::new();
    }
    let (on, dim, off) = if color {
        ("\x1b[1;36m", "\x1b[2m", "\x1b[0m")
    } else {
        ("", "", "")
    };
    let mut side = vec![format!("swagsh {}", env!("CARGO_PKG_VERSION"))];
    if banner.tips {
        side.push(format!("{dim}tip: {}{off}", pick(TIPS)));
    }
    let mut out = String::new();
    for (i, line) in LOGO.iter().enumerate() {
        let _ = write!(out, "{on}{line}{off}");
        if let Some(text) = i.checked_sub(TEXT_ROW).and_then(|i| side.get(i)) {
            out.push_str("  ");
            out.push_str(text);
        }
        out.push('\n');
    }
    out
}

/// One of `items`, different from one start to the next.
fn pick<'a>(items: &[&'a str]) -> &'a str {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    items[nanos as usize % items.len()]
}
