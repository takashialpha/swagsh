//! What the prompt shows of the git repository it is in: the branch, read
//! from `.git` without running git, and the state of the work tree, from
//! `git status`.

use std::fmt::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::env::Env;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Info {
    /// The branch, or the first digits of the commit when none is checked
    /// out.
    pub branch: String,
    pub changed: bool,
    pub staged: bool,
    pub untracked: bool,
    pub ahead: u64,
    pub behind: u64,
}

impl Info {
    /// The branch and its state as the prompt shows them: `main *+?↑1↓2`.
    #[must_use]
    pub fn render(&self) -> String {
        let mut marks = String::new();
        for (on, mark) in [
            (self.changed, '*'),
            (self.staged, '+'),
            (self.untracked, '?'),
        ] {
            if on {
                marks.push(mark);
            }
        }
        if self.ahead > 0 {
            let _ = write!(marks, "↑{}", self.ahead);
        }
        if self.behind > 0 {
            let _ = write!(marks, "↓{}", self.behind);
        }
        if marks.is_empty() {
            self.branch.clone()
        } else {
            format!("{} {marks}", self.branch)
        }
    }
}

/// The repository `dir` is in, if any; with `status`, its state too, when
/// git can be found on the shell's `$PATH`.
#[must_use]
pub fn info(dir: &Path, status: bool, env: &Env) -> Option<Info> {
    let git_dir = git_dir(dir)?;
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    let branch = head.strip_prefix("ref: ").map_or_else(
        || head.get(..7).unwrap_or(head).to_owned(),
        |r| r.strip_prefix("refs/heads/").unwrap_or(r).to_owned(),
    );
    let mut info = Info {
        branch,
        ..Info::default()
    };
    if status && let Some(git) = find_git(env) {
        read_status(&git, dir, env, &mut info);
    }
    Some(info)
}

/// The `.git` directory of the repository `dir` is in. In a linked work
/// tree or a submodule, `.git` is a file naming it.
fn git_dir(dir: &Path) -> Option<PathBuf> {
    for d in dir.ancestors() {
        let dot = d.join(".git");
        if dot.is_dir() {
            return Some(dot);
        }
        if let Ok(text) = std::fs::read_to_string(&dot) {
            return text.strip_prefix("gitdir:").map(|path| d.join(path.trim()));
        }
    }
    None
}

fn find_git(env: &Env) -> Option<PathBuf> {
    env.get("PATH")?
        .split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| Path::new(dir).join("git"))
        .find(|path| {
            std::fs::metadata(path)
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

/// Fills in the state of the work tree from `git status`, run with the
/// shell's exported variables.
fn read_status(git: &Path, dir: &Path, env: &Env, info: &mut Info) {
    let output = Command::new(git)
        .args([
            "--no-optional-locks",
            "status",
            "--porcelain=v2",
            "--branch",
        ])
        .current_dir(dir)
        .env_clear()
        .envs(env.exported())
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let Ok(output) = output else { return };
    if output.status.success() {
        parse_status(&String::from_utf8_lossy(&output.stdout), info);
    }
}

/// Reads `git status --porcelain=v2 --branch` output into `info`.
fn parse_status(text: &str, info: &mut Info) {
    for line in text.lines() {
        if let Some(counts) = line.strip_prefix("# branch.ab ") {
            for count in counts.split_whitespace() {
                if let Some(n) = count.strip_prefix('+') {
                    info.ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = count.strip_prefix('-') {
                    info.behind = n.parse().unwrap_or(0);
                }
            }
        } else if let Some(entry) = line.strip_prefix("1 ").or_else(|| line.strip_prefix("2 ")) {
            let xy = entry.as_bytes();
            info.staged |= xy.first().is_some_and(|&x| x != b'.');
            info.changed |= xy.get(1).is_some_and(|&y| y != b'.');
        } else if line.starts_with("u ") {
            info.changed = true;
        } else if line.starts_with("? ") {
            info.untracked = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_marks() {
        let mut info = Info {
            branch: "main".to_owned(),
            ..Info::default()
        };
        parse_status(
            "# branch.oid abc\n# branch.head main\n# branch.ab +2 -1\n\
             1 .M N... 100644 100644 100644 a b file\n\
             1 A. N... 000000 100644 100644 a b new\n? junk\n",
            &mut info,
        );
        assert_eq!(info.render(), "main *+?↑2↓1");
    }
}
