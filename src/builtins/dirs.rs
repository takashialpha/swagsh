use anyhow::{Result, anyhow};
use clap::Parser;

use crate::errfmt::strerror;
use crate::eval::Shell;
use crate::expand::expand_tilde;
use crate::jobs::ExitStatus;

use super::Builtin;

/// Collapses `.`/`..` components in `base/target` purely as text, without
/// touching the filesystem, so a directory reached through a symlink keeps
/// the traversed (not resolved) path in `$PWD` by default: the same
/// `-L`/`-P` distinction `cd`/`pwd` make.
fn normalize_logical(base: &str, target: &str) -> String {
    let joined = if target.starts_with('/') {
        target.to_owned()
    } else if base.is_empty() {
        format!("/{target}")
    } else {
        format!("{base}/{target}")
    };
    let mut stack: Vec<&str> = Vec::new();
    for comp in joined.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                stack.pop();
            }
            c => stack.push(c),
        }
    }
    format!("/{}", stack.join("/"))
}

#[derive(Parser)]
#[command(name = "cd", about = "Change the shell working directory")]
// Each bool is an independent clap-derived CLI flag, always constructed by
// clap from named flags rather than positionally.
#[allow(clippy::struct_excessive_bools)]
pub struct CdBuiltin {
    /// Resolve symlinks after processing `..` in DIR (default)
    #[arg(short = 'L', overrides_with = "physical")]
    logical: bool,
    /// Resolve symlinks before processing `..`, so $PWD never names a symlink
    #[arg(short = 'P', overrides_with = "logical")]
    physical: bool,
    /// With -P, exit non-zero if the new working directory can't be read back
    #[arg(short = 'e')]
    exit_on_fail: bool,
    /// Accepted for compatibility; extended-attribute directories aren't a Linux concept
    #[arg(short = '@')]
    xattr: bool,
    /// The directory to go to: $HOME when left out, the previous one for `-`
    dir: Option<String>,
}

impl Builtin for CdBuiltin {
    fn run(self, shell: &mut Shell) -> Result<ExitStatus> {
        let _ = self.xattr;
        let physical = self.physical && !self.logical;

        let (target, print_target) = match self.dir.as_deref() {
            Some("-") => (shell.env.get("OLDPWD").unwrap_or_else(|| "/".into()), true),
            Some(path) => search_cdpath(shell, &expand_tilde(path, &shell.env)),
            None => (shell.env.get("HOME").unwrap_or_else(|| "/".into()), false),
        };
        match change_dir(shell, &target, physical)? {
            Some(new_pwd) if print_target => shell.outln(&new_pwd)?,
            None if self.exit_on_fail => return Ok(ExitStatus::FAILURE),
            _ => {}
        }
        Ok(ExitStatus::SUCCESS)
    }
}

/// POSIX `CDPATH`: a relative DIR that does not start with `.` or `..` is
/// looked for under each `CDPATH` entry in turn. Returns the directory to
/// use, and whether it was found through a non-empty entry, in which case
/// `cd` prints where it went.
fn search_cdpath(shell: &Shell, dir: &str) -> (String, bool) {
    let first = dir.split('/').next().unwrap_or("");
    let Some(cdpath) = shell
        .env
        .get("CDPATH")
        .filter(|_| !dir.starts_with('/') && first != "." && first != "..")
    else {
        return (dir.to_owned(), false);
    };
    for entry in cdpath.split(':') {
        let candidate = if entry.is_empty() {
            dir.to_owned()
        } else {
            format!("{}/{dir}", entry.trim_end_matches('/'))
        };
        if std::path::Path::new(&candidate).is_dir() {
            return (candidate, !entry.is_empty());
        }
    }
    (dir.to_owned(), false)
}

/// Changes to `target`, updating `$PWD` and `$OLDPWD`, and returns the new
/// `$PWD`. Logically (the default) `..` is resolved against `$PWD` as
/// text, so a symlink traversed stays in the path; physically, the kernel
/// resolves `target` as given and `$PWD` is read back from it. `None` when
/// the directory changed but `-P` could not read the new path back.
///
/// # Errors
///
/// Returns an error if the directory cannot be entered.
pub(super) fn change_dir(
    shell: &mut Shell,
    target: &str,
    physical: bool,
) -> Result<Option<String>> {
    let old_pwd = shell.env.get("PWD").unwrap_or_default();
    let dest = if physical {
        target.to_owned()
    } else {
        normalize_logical(&old_pwd, target)
    };
    std::env::set_current_dir(&dest).map_err(|e| anyhow!("cd: {target}: {}", strerror(e)))?;
    shell.env.export("OLDPWD", old_pwd);
    let new_pwd = if physical {
        match std::env::current_dir() {
            Ok(p) => p.to_string_lossy().into_owned(),
            Err(_) => return Ok(None),
        }
    } else {
        dest
    };
    shell.env.export("PWD", &new_pwd);
    Ok(Some(new_pwd))
}

/// `$PWD` with `$HOME` shown as `~`, the way `dirs` lists it.
fn tilde_abbreviate(shell: &Shell, dir: &str) -> String {
    match shell.env.get("HOME").filter(|h| !h.is_empty() && h != "/") {
        Some(home) if dir == home => "~".to_owned(),
        Some(home) => dir
            .strip_prefix(&format!("{home}/"))
            .map_or_else(|| dir.to_owned(), |rest| format!("~/{rest}")),
        None => dir.to_owned(),
    }
}

/// The directory stack as `dirs` shows it: the current directory, then the
/// saved ones, most recent first.
fn dir_stack(shell: &Shell) -> Vec<String> {
    let mut all = vec![shell.env.get("PWD").unwrap_or_default()];
    all.extend(shell.dir_stack.iter().cloned());
    all
}

/// Parses a `+N`/`-N` stack index (counted from the left or the right of
/// `dirs`' listing) into an index into it.
fn stack_index(spec: &str, len: usize) -> Option<usize> {
    let (from_right, n) = match spec.as_bytes().first() {
        Some(b'+') => (false, &spec[1..]),
        Some(b'-') => (true, &spec[1..]),
        _ => return None,
    };
    let n: usize = n.parse().ok()?;
    if n >= len {
        return None;
    }
    Some(if from_right { len - 1 - n } else { n })
}

fn print_dirs(shell: &mut Shell) -> Result<()> {
    let line = dir_stack(shell)
        .iter()
        .map(|d| tilde_abbreviate(shell, d))
        .collect::<Vec<_>>()
        .join(" ");
    shell.outln(&line)
}

#[derive(Parser)]
#[command(name = "dirs", about = "Show the directory stack")]
// Each bool is an independent clap-derived CLI flag.
#[allow(clippy::struct_excessive_bools)]
pub struct DirsBuiltin {
    /// Clear the stack
    #[arg(short = 'c')]
    clear: bool,
    /// Show full paths instead of abbreviating `$HOME` as `~`
    #[arg(short = 'l')]
    long: bool,
    /// One entry per line
    #[arg(short = 'p')]
    per_line: bool,
    /// One entry per line, numbered
    #[arg(short = 'v')]
    verbose: bool,
}

impl Builtin for DirsBuiltin {
    fn run(self, shell: &mut Shell) -> Result<ExitStatus> {
        if self.clear {
            shell.dir_stack.clear();
            return Ok(ExitStatus::SUCCESS);
        }
        let entries: Vec<String> = dir_stack(shell)
            .iter()
            .map(|d| {
                if self.long {
                    d.clone()
                } else {
                    tilde_abbreviate(shell, d)
                }
            })
            .collect();
        if self.verbose {
            for (i, d) in entries.iter().enumerate() {
                shell.outln(&format!("{i:2}  {d}"))?;
            }
        } else if self.per_line {
            for d in &entries {
                shell.outln(d)?;
            }
        } else {
            shell.outln(&entries.join(" "))?;
        }
        Ok(ExitStatus::SUCCESS)
    }
}

#[derive(Parser)]
#[command(
    name = "pushd",
    about = "Change directory, saving the current one on the stack",
    allow_hyphen_values = true
)]
pub struct PushdBuiltin {
    /// A directory, or +N/-N to rotate that stack entry to the top; with
    /// neither, swap the top two
    target: Option<String>,
}

impl Builtin for PushdBuiltin {
    fn run(self, shell: &mut Shell) -> Result<ExitStatus> {
        let mut stack = dir_stack(shell);
        let dir = match self.target.as_deref() {
            None => {
                if stack.len() < 2 {
                    return Err(anyhow!("pushd: no other directory"));
                }
                stack.swap(0, 1);
                stack[0].clone()
            }
            Some(spec) if spec.len() > 1 && spec.starts_with(['+', '-']) => {
                let i = stack_index(spec, stack.len())
                    .ok_or_else(|| anyhow!("pushd: {spec}: directory stack index out of range"))?;
                stack.rotate_left(i);
                stack[0].clone()
            }
            Some(dir) => {
                let dir = expand_tilde(dir, &shell.env).into_owned();
                change_dir(shell, &dir, false).map_err(|e| anyhow!("pushd: {e}"))?;
                shell.dir_stack.insert(0, stack[0].clone());
                print_dirs(shell)?;
                return Ok(ExitStatus::SUCCESS);
            }
        };
        change_dir(shell, &dir, false).map_err(|e| anyhow!("pushd: {e}"))?;
        shell.dir_stack = stack[1..].to_vec();
        print_dirs(shell)?;
        Ok(ExitStatus::SUCCESS)
    }
}

#[derive(Parser)]
#[command(
    name = "popd",
    about = "Remove a directory from the stack, changing to the new top",
    allow_hyphen_values = true
)]
pub struct PopdBuiltin {
    /// +N/-N removes that stack entry instead of the top
    target: Option<String>,
}

impl Builtin for PopdBuiltin {
    fn run(self, shell: &mut Shell) -> Result<ExitStatus> {
        if shell.dir_stack.is_empty() {
            return Err(anyhow!("popd: directory stack empty"));
        }
        let stack = dir_stack(shell);
        let i = match self.target.as_deref() {
            None => 0,
            Some(spec) => stack_index(spec, stack.len())
                .ok_or_else(|| anyhow!("popd: {spec}: directory stack index out of range"))?,
        };
        if i == 0 {
            let next = shell.dir_stack.remove(0);
            if let Err(e) = change_dir(shell, &next, false) {
                shell.dir_stack.insert(0, next);
                return Err(anyhow!("popd: {e}"));
            }
        } else {
            shell.dir_stack.remove(i - 1);
        }
        print_dirs(shell)?;
        Ok(ExitStatus::SUCCESS)
    }
}

#[derive(Parser)]
#[command(name = "pwd", about = "Print the current working directory")]
pub struct PwdBuiltin {
    /// Print $PWD as-is, without resolving symlinks (default)
    #[arg(short = 'L', overrides_with = "physical")]
    logical: bool,
    /// Resolve all symlinks before printing
    #[arg(short = 'P', overrides_with = "logical")]
    physical: bool,
}

impl Builtin for PwdBuiltin {
    fn run(self, shell: &mut Shell) -> Result<ExitStatus> {
        let physical = self.physical && !self.logical;
        if !physical && let Some(pwd) = shell.env.get("PWD").filter(|p| !p.is_empty()) {
            shell.outln(&pwd)?;
            return Ok(ExitStatus::SUCCESS);
        }
        let cwd = std::env::current_dir().map_err(|e| anyhow!("pwd: {}", strerror(e)))?;
        shell.outln(&cwd.display().to_string())?;
        Ok(ExitStatus::SUCCESS)
    }
}
