//! Job listing and foreground/background movement (`jobs`, `fg`, `bg`).
//! `kill` and `wait` also operate on the job table but each has enough of
//! its own machinery (a signal-name table, a wait-for-completion loop) to
//! warrant its own file; `find_job`/`parse_job_id` here are specific to
//! `fg`/`bg`'s `%job`-only argument, `wait.rs` has its own pid-or-jobspec
//! lookup since it additionally accepts a raw pid.

use anyhow::{Result, anyhow};
use clap::Parser;
use rustix::io::Errno;
use rustix::process::{Signal, WaitOptions, kill_process_group, waitpgid};
use rustix::termios::tcsetpgrp;

use crate::eval::Shell;
use crate::jobs::{ExitStatus, Job, JobState, decode_wait_status};

use super::Builtin;

/// Parses the `%job` argument `fg`/`bg` take, defaulting to job 1 (`%%`,
/// the "current job", which this shell doesn't distinguish from job 1).
/// A job id from `%N`, `N`, or the current job (`%%`, `%+`, or none given:
/// the most recently started), or the previous one (`%-`).
pub(super) fn parse_job_id(shell: &Shell, job: Option<&str>) -> usize {
    let mut ids: Vec<usize> = shell.jobs.iter().map(|j| j.id).collect();
    ids.sort_unstable();
    match job.map(|s| s.strip_prefix('%').unwrap_or(s)) {
        None | Some("%" | "+" | "") => ids.last().copied().unwrap_or(0),
        Some("-") => ids.iter().rev().nth(1).copied().unwrap_or(0),
        Some(n) => n.parse().unwrap_or(0),
    }
}

pub(super) fn find_job(shell: &Shell, id: usize) -> Option<&Job> {
    shell.jobs.iter().find(|j| j.id == id)
}

#[derive(Parser)]
#[command(name = "jobs", about = "List active jobs")]
// Each bool is an independent clap-derived CLI flag (`-l`/`-p`/`-r`/`-s`),
// always constructed by clap from named flags rather than positionally, so
// there's no risk of the "which bool means what" confusion this lint guards
// against.
#[allow(clippy::struct_excessive_bools)]
pub struct JobsBuiltin {
    /// Also list each job's process group id
    #[arg(short = 'l')]
    long: bool,
    /// List only process ids
    #[arg(short = 'p')]
    pids_only: bool,
    /// List only running jobs
    #[arg(short = 'r')]
    running_only: bool,
    /// List only stopped jobs
    #[arg(short = 's')]
    stopped_only: bool,
}

impl Builtin for JobsBuiltin {
    fn run(self, shell: &mut Shell) -> Result<ExitStatus> {
        shell.jobs.reap_nonblocking();
        let lines: Vec<String> = shell
            .jobs
            .iter()
            .filter(|j| {
                (!self.running_only || matches!(j.state, JobState::Running))
                    && (!self.stopped_only || matches!(j.state, JobState::Stopped))
                    && !(j.notified && matches!(j.state, JobState::Done(_)))
            })
            .map(|j| {
                if self.pids_only {
                    j.pgid.to_string()
                } else {
                    shell.jobs.describe(j, self.long)
                }
            })
            .collect();
        for line in lines {
            shell.outln(&line)?;
        }
        // POSIX: a finished job is reported once, then forgotten.
        let _ = shell.jobs.take_finished();
        Ok(ExitStatus::SUCCESS)
    }
}

// `fg`/`bg`'s `job` field deliberately doesn't set `allow_hyphen_values`:
// real jobspecs are `%`-prefixed (`%1`, `%%`, `%+`, ...), never `-`-led, so
// leaving clap's default flag-vs-positional heuristic in place means a
// bogus `-x` is rejected as an unrecognized flag (a UsageError, exit 2)
// instead of silently accepted as jobspec data, closer to `fg -x` being an
// "invalid option" than treating it as a job to look up.

#[derive(Parser)]
#[command(name = "fg", about = "Move a job to the foreground")]
pub struct FgBuiltin {
    job: Option<String>,
}

impl Builtin for FgBuiltin {
    fn run(self, shell: &mut Shell) -> Result<ExitStatus> {
        if !shell.monitor {
            return Err(anyhow!("fg: no job control"));
        }
        let id = parse_job_id(shell, self.job.as_deref());
        let (pgid, cmd) = find_job(shell, id)
            .map(|j| (j.pgid, j.command.clone()))
            .ok_or_else(|| anyhow!("fg: no such job: {id}"))?;
        eprintln!("{cmd}");
        kill_process_group(pgid, Signal::CONT)?;
        let _ = tcsetpgrp(std::io::stdin(), pgid);
        let status = loop {
            match waitpgid(pgid, WaitOptions::UNTRACED) {
                Ok(Some((_, ws))) => {
                    if let Some(exit) = decode_wait_status(ws) {
                        break exit;
                    } else if ws.stopped() {
                        let _ = tcsetpgrp(std::io::stdin(), shell.pgid);
                        shell.restore_terminal();
                        break ExitStatus(130);
                    }
                }
                Ok(None) => {}
                Err(e) if e == Errno::INTR => {}
                Err(e) => return Err(e.into()),
            }
        };
        let _ = tcsetpgrp(std::io::stdin(), shell.pgid);
        shell.restore_terminal();
        shell.jobs.remove(id);
        Ok(status)
    }
}

#[derive(Parser)]
#[command(name = "bg", about = "Move a job to the background")]
pub struct BgBuiltin {
    job: Option<String>,
}

impl Builtin for BgBuiltin {
    fn run(self, shell: &mut Shell) -> Result<ExitStatus> {
        if !shell.monitor {
            return Err(anyhow!("bg: no job control"));
        }
        let id = parse_job_id(shell, self.job.as_deref());
        let pgid = find_job(shell, id)
            .map(|j| j.pgid)
            .ok_or_else(|| anyhow!("bg: no such job: {id}"))?;
        kill_process_group(pgid, Signal::CONT)?;
        if let Some(job) = shell.jobs.by_pgid_mut(pgid) {
            job.state = JobState::Running;
            job.stop_announced = false;
        }
        eprintln!("[{id}]+ {pgid} &");
        Ok(ExitStatus::SUCCESS)
    }
}

#[derive(Parser)]
#[command(name = "disown", about = "Remove jobs from the job table")]
pub struct DisownBuiltin {
    /// Remove every job
    #[arg(short = 'a')]
    all: bool,
    jobs: Vec<String>,
}

impl Builtin for DisownBuiltin {
    fn run(self, shell: &mut Shell) -> Result<ExitStatus> {
        let ids: Vec<usize> = if self.all {
            shell.jobs.iter().map(|j| j.id).collect()
        } else if self.jobs.is_empty() {
            vec![parse_job_id(shell, None)]
        } else {
            self.jobs
                .iter()
                .map(|j| parse_job_id(shell, Some(j)))
                .collect()
        };
        for id in ids {
            if find_job(shell, id).is_none() {
                return Err(anyhow!("disown: no such job: {id}"));
            }
            shell.jobs.remove(id);
        }
        Ok(ExitStatus::SUCCESS)
    }
}
