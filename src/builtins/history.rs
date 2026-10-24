//! `history` and `fc`: the interactive command history, which the REPL
//! keeps in [`Shell::history`].

use anyhow::{Result, anyhow, bail};
use clap::Parser;

use super::Builtin;
use crate::eval::Shell;
use crate::jobs::ExitStatus;

#[derive(Parser)]
#[command(name = "history", about = "Show or edit the command history")]
pub struct HistoryBuiltin {
    /// Clear the history
    #[arg(short = 'c')]
    clear: bool,
    /// Delete entry N
    #[arg(short = 'd', value_name = "N")]
    delete: Option<usize>,
    /// Show only the last COUNT entries
    count: Option<usize>,
}

impl Builtin for HistoryBuiltin {
    fn run(self, shell: &mut Shell) -> Result<ExitStatus> {
        if self.clear {
            shell.history.clear();
            shell.history_changed = true;
            return Ok(ExitStatus::SUCCESS);
        }
        if let Some(n) = self.delete {
            if n == 0 || n > shell.history.len() {
                bail!("history: {n}: history position out of range");
            }
            shell.history.remove(n - 1);
            shell.history_changed = true;
            return Ok(ExitStatus::SUCCESS);
        }
        let skip = self
            .count
            .map_or(0, |count| shell.history.len().saturating_sub(count));
        let entries: Vec<(usize, String)> = shell
            .history
            .iter()
            .cloned()
            .enumerate()
            .skip(skip)
            .collect();
        for (i, entry) in entries {
            shell.outln(&format!("{:5}  {entry}", i + 1))?;
        }
        Ok(ExitStatus::SUCCESS)
    }
}

#[derive(Parser)]
#[command(
    name = "fc",
    about = "List, edit and re-run commands from the history",
    allow_negative_numbers = true
)]
// Each bool is an independent clap-derived CLI flag.
#[allow(clippy::struct_excessive_bools)]
pub struct FcBuiltin {
    /// List the commands instead of editing them
    #[arg(short = 'l')]
    list: bool,
    /// With -l, leave out the numbers
    #[arg(short = 'n')]
    no_numbers: bool,
    /// Reverse the order
    #[arg(short = 'r')]
    reverse: bool,
    /// Re-run a command without editing it: `fc -s [OLD=NEW] [FIRST]`
    #[arg(short = 's')]
    rerun: bool,
    /// The editor to use instead of `$FCEDIT`
    #[arg(short = 'e', value_name = "EDITOR")]
    editor: Option<String>,
    /// FIRST and LAST: a history number, a negative offset from the most
    /// recent command, or the start of a command's text
    operands: Vec<String>,
}

impl Builtin for FcBuiltin {
    fn run(self, shell: &mut Shell) -> Result<ExitStatus> {
        // The `fc` command itself is the newest entry when typed at the
        // prompt, and does not count.
        let len = shell
            .history
            .len()
            .saturating_sub(usize::from(shell.interactive));
        let history = &shell.history[..len];
        if self.rerun {
            let (replace, first) = match self.operands.as_slice() {
                [] => (None, None),
                [one] if one.contains('=') => (Some(one.as_str()), None),
                [one] => (None, Some(one.as_str())),
                [r, f, ..] => (Some(r.as_str()), Some(f.as_str())),
            };
            let index = find_entry(history, first.unwrap_or("-1"))?;
            let mut command = history[index].clone();
            if let Some((old, new)) = replace.and_then(|r| r.split_once('=')) {
                command = command.replacen(old, new, 1);
            }
            return rerun(shell, &command);
        }
        let default_first = if self.list { "-16" } else { "-1" };
        let first = find_entry(
            history,
            self.operands.first().map_or(default_first, String::as_str),
        )?;
        let last = match self.operands.get(1) {
            Some(spec) => find_entry(history, spec)?,
            None if self.list => len.saturating_sub(1),
            None => first,
        };
        let (lo, hi) = (first.min(last), first.max(last));
        let mut range: Vec<(usize, String)> = (lo..=hi).map(|i| (i, history[i].clone())).collect();
        if self.reverse != (first > last) {
            range.reverse();
        }
        if self.list {
            for (i, entry) in range {
                if self.no_numbers {
                    shell.outln(&format!("\t{entry}"))?;
                } else {
                    shell.outln(&format!("{}\t{entry}", i + 1))?;
                }
            }
            return Ok(ExitStatus::SUCCESS);
        }
        let editor = self
            .editor
            .or_else(|| shell.env.get("FCEDIT"))
            .unwrap_or_else(|| "ed".to_owned());
        let commands: Vec<String> = range.into_iter().map(|(_, c)| c).collect();
        let edited = edit_in(shell, &editor, &(commands.join("\n") + "\n"))?;
        rerun(shell, edited.trim_end())
    }
}

/// Resolves an `fc` operand to an index into `history`.
fn find_entry(history: &[String], spec: &str) -> Result<usize> {
    if history.is_empty() {
        bail!("fc: history is empty");
    }
    let last = history.len() - 1;
    if let Ok(n) = spec.parse::<i64>() {
        let index = if n < 0 {
            usize::try_from(n.unsigned_abs())
                .ok()
                .and_then(|back| history.len().checked_sub(back))
        } else {
            usize::try_from(n).ok().and_then(|n| n.checked_sub(1))
        };
        // Out-of-range numbers clamp to the nearest end, as POSIX allows.
        return Ok(index.map_or(0, |i| i.min(last)));
    }
    history
        .iter()
        .rposition(|entry| entry.starts_with(spec))
        .ok_or_else(|| anyhow!("fc: {spec}: no command found"))
}

/// Echoes and runs `command`, which takes the `fc` command's place in the
/// history.
fn rerun(shell: &mut Shell, command: &str) -> Result<ExitStatus> {
    shell.outln(command)?;
    if shell.interactive {
        shell.history.pop();
        shell.history.push(command.to_owned());
        shell.history_changed = true;
    }
    shell.run_source(command, "fc: ")
}

/// Writes `text` to a temporary file, runs `editor` on it, and returns what
/// the file holds afterwards.
fn edit_in(shell: &mut Shell, editor: &str, text: &str) -> Result<String> {
    let path = std::env::temp_dir().join(format!("swagsh-fc-{}", std::process::id()));
    std::fs::write(&path, text).map_err(|e| anyhow!("fc: {}: {e}", path.display()))?;
    let command = format!(
        "{editor} {}",
        crate::expand::shell_quote(&path.to_string_lossy())
    );
    let status = shell.run_source(&command, "fc: ");
    let edited = std::fs::read_to_string(&path);
    let _ = std::fs::remove_file(&path);
    if !status?.is_success() {
        bail!("fc: {editor} failed");
    }
    edited.map_err(|e| anyhow!("fc: {e}"))
}
