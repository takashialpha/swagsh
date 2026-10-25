use std::io::Write;

use anyhow::{Result, anyhow};
use clap::Parser;
use rustix::termios::{LocalModes, OptionalActions, Termios, tcgetattr, tcsetattr};

use crate::errfmt::strerror;
use crate::eval::{ExpandedAssign, ExpandedValue, Shell};
use crate::fd::{read_raw, seek_back};
use crate::jobs::ExitStatus;
use rustix::fd::RawFd;

use super::Builtin;

/// Toggles the terminal's `ECHO` flag off for the duration of `read -s`,
/// restoring it on drop regardless of how the caller returns (including via
/// `?`). A no-op when stdin isn't a terminal.
struct EchoGuard(Option<Termios>);

impl EchoGuard {
    fn disable() -> Self {
        let Ok(saved) = tcgetattr(std::io::stdin()) else {
            return Self(None);
        };
        let mut raw = saved.clone();
        raw.local_modes.remove(LocalModes::ECHO);
        let _ = tcsetattr(std::io::stdin(), OptionalActions::Now, &raw);
        Self(Some(saved))
    }

    /// Whether this guard actually suppressed terminal echo (stdin is a
    /// real terminal), as opposed to being a no-op because stdin is piped:
    /// the caller only owes the user a compensating newline in the former
    /// case, since piped input was never echoed in the first place.
    const fn is_active(&self) -> bool {
        self.0.is_some()
    }
}

impl Drop for EchoGuard {
    fn drop(&mut self) {
        if let Some(saved) = &self.0 {
            let _ = tcsetattr(std::io::stdin(), OptionalActions::Now, saved);
        }
    }
}

/// Reads raw bytes from descriptor `fd` up to (and excluding) `delim`,
/// never consuming input past it, which would strand what a later command
/// (or a second `read`) still needs: a file is read in blocks and its
/// offset moved back over what was not used, anything else (a pipe, a
/// terminal) one byte at a time. In non-raw mode, a backslash immediately
/// before `delim` escapes it: both bytes are dropped and reading continues,
/// matching a shell's usual backslash-newline continuation but generalized
/// to `-d`'s delimiter.
///
/// Returns the bytes read and whether `delim` was actually seen (`false`
/// means EOF cut the read short, the caller's cue to report failure while
/// still assigning whatever partial data came through).
pub(super) fn read_until_delim(
    fd: RawFd,
    delim: u8,
    raw: bool,
) -> std::io::Result<(Vec<u8>, bool)> {
    let mut out = Vec::new();
    let mut block = [0u8; 512];
    // Probed with an empty seek: only a file can take its input back.
    let size = if seek_back(fd, 0) { block.len() } else { 1 };
    loop {
        let n = match read_raw(fd, &mut block[..size]) {
            Ok(0) => return Ok((out, false)),
            Ok(n) => n,
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => return Err(e.into()),
        };
        let mut i = 0;
        while i < n {
            let b = block[i];
            i += 1;
            if b != delim {
                out.push(b);
                continue;
            }
            if !raw && out.last() == Some(&b'\\') {
                out.pop();
                continue;
            }
            if i < n {
                seek_back(fd, n - i);
            }
            return Ok((out, true));
        }
    }
}

/// Splits `line` into exactly `n_vars` fields on `ifs`, the way `read`
/// assigns its trailing variable everything left over (IFS included) rather
/// than continuing to split; with `None`, into as many fields as there are
/// (`read -a`). In non-raw mode a backslash makes the next character
/// literal, both stripped from the output and exempted from acting as a
/// field separator.
fn split_fields(line: &str, ifs: &str, n_vars: Option<usize>, raw: bool) -> Vec<String> {
    let is_ifs = |c: char| ifs.contains(c);
    let all = n_vars.is_none();
    let n_vars = n_vars.unwrap_or(usize::MAX);
    let mut fields = Vec::new();
    let mut cur = String::new();
    let mut chars = line.chars().peekable();
    while chars.peek().is_some_and(|&c| is_ifs(c)) {
        chars.next();
    }
    while let Some(c) = chars.next() {
        if !raw && c == '\\' {
            if let Some(next) = chars.next() {
                cur.push(next);
            }
            continue;
        }
        if is_ifs(c) && fields.len() + 1 < n_vars {
            fields.push(std::mem::take(&mut cur));
            while chars.peek().is_some_and(|&c| is_ifs(c)) {
                chars.next();
            }
            continue;
        }
        cur.push(c);
    }
    let trimmed = cur.trim_end_matches(is_ifs).len();
    cur.truncate(trimmed);
    if !(all && cur.is_empty()) {
        fields.push(cur);
    }
    if !all {
        fields.resize(n_vars, String::new());
    }
    fields
}

#[derive(Parser)]
#[command(
    name = "read",
    about = "Read a line from standard input into variables"
)]
pub struct ReadBuiltin {
    /// Don't treat backslash as an escape character
    #[arg(short = 'r')]
    raw: bool,
    /// Print PROMPT to stderr before reading
    #[arg(short = 'p')]
    prompt: Option<String>,
    /// Disable terminal echo while reading (e.g. for a password)
    #[arg(short = 's')]
    silent: bool,
    /// Read until the first character of DELIM instead of newline
    #[arg(short = 'd')]
    delim: Option<String>,
    /// Assign the words read to the indexed array ARRAY, one per element
    #[arg(short = 'a', value_name = "ARRAY")]
    array: Option<String>,
    names: Vec<String>,
}

impl Builtin for ReadBuiltin {
    fn run(self, shell: &mut Shell) -> Result<ExitStatus> {
        let delim = self
            .delim
            .as_deref()
            .and_then(|d| d.bytes().next())
            .unwrap_or(b'\n');

        if let Some(prompt) = &self.prompt {
            eprint!("{prompt}");
            let _ = std::io::stderr().flush();
        }

        let echo_guard = self.silent.then(EchoGuard::disable);
        let (raw_bytes, hit_delim) =
            read_until_delim(0, delim, self.raw).map_err(|e| anyhow!("read: {}", strerror(e)))?;
        if echo_guard.is_some_and(|g| g.is_active()) {
            shell.outln("")?;
        } else if hit_delim && delim == b'\n' {
            // No `-s`, so the terminal's own local echo already showed the
            // newline the user typed to submit this line; nothing of ours
            // to flush, but the cursor really is at a fresh line now.
            shell.note_stdout("\n");
        }
        let line = String::from_utf8_lossy(&raw_bytes).into_owned();

        let ifs = shell.env.get("IFS").unwrap_or_else(|| " \t\n".to_owned());
        if let Some(array) = self.array {
            let items = split_fields(&line, &ifs, None, self.raw)
                .into_iter()
                .map(|v| (None, false, v))
                .collect();
            let assign = ExpandedAssign {
                name: array,
                index: None,
                append: false,
                value: ExpandedValue::Array(items),
            };
            shell
                .apply_assign(&assign)
                .map_err(|e| anyhow!("read: {e}"))?;
            return Ok(if hit_delim {
                ExitStatus::SUCCESS
            } else {
                ExitStatus::FAILURE
            });
        }
        let names: Vec<&str> = if self.names.is_empty() {
            vec!["REPLY"]
        } else {
            self.names.iter().map(String::as_str).collect()
        };
        let fields = split_fields(&line, &ifs, Some(names.len()), self.raw);
        for (name, value) in names.iter().zip(fields) {
            shell
                .assign_target(name, value)
                .map_err(|e| anyhow!("read: {e}"))?;
        }

        Ok(if hit_delim {
            ExitStatus::SUCCESS
        } else {
            ExitStatus::FAILURE
        })
    }
}
