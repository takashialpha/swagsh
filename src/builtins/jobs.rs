//! Job listing and foreground/background movement (`jobs`, `fg`, `bg`).
//! `kill` and `wait` also operate on the job table but each has enough of
//! its own machinery (a signal-name table, a wait-for-completion loop) to
//! warrant its own file; `find_job`/`parse_job_id` here are specific to
//! `fg`/`bg`'s `%job`-only argument, `wait.rs` has its own pid-or-jobspec
//! lookup since it additionally accepts a raw pid.

use anyhow::{Result, anyhow};
use clap::Parser;
use rustix::io::Errno;
use rustix::process::{Pid, Signal, WaitOptions, kill_process_group, waitpgid};
use rustix::termios::tcsetpgrp;

use crate::eval::Shell;
use crate::jobs::{ExitStatus, Job, JobState, decode_wait_status};

use super::Builtin;

/// Parses the `%job` argument `fg`/`bg` take, defaulting to job 1 (`%%`,
/// the "current job", which this shell doesn't distinguish from job 1).
pub(super) fn parse_job_id(job: Option<&str>) -> usize {
    job.and_then(|s| s.trim_start_matches('%').parse().ok())
        .unwrap_or(1)
}

pub(super) fn find_job(shell: &Shell, id: usize) -> Option<&Job> {
    shell.jobs.iter().find(|j| j.id == id)
}

#[derive(Parser)]
#[command(name = "jobs", about = "List active jobs")]
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
        let entries: Vec<(usize, Pid, String, String)> = shell
            .jobs
            .iter()
            .filter(|j| {
                (!self.running_only || matches!(j.state, JobState::Running))
                    && (!self.stopped_only || matches!(j.state, JobState::Stopped))
            })
            .map(|j| {
                let state = match &j.state {
                    JobState::Running => "Running".into(),
                    JobState::Stopped => "Stopped".into(),
                    JobState::Done(s) => format!("Done({})", s.0),
                };
                (j.id, j.pgid, state, j.command.clone())
            })
            .collect();
        if !entries.is_empty() {
            shell.note_stdout("\n");
        }
        for (id, pgid, state, cmd) in entries {
            if self.pids_only {
                println!("{pgid}");
            } else if self.long {
                println!("[{id}]  {pgid}  {state}\t{cmd}");
            } else {
                println!("[{id}]  {state}\t{cmd}");
            }
        }
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
        if !shell.interactive {
            return Err(anyhow!("fg: no job control"));
        }
        let id = parse_job_id(self.job.as_deref());
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
        if !shell.interactive {
            return Err(anyhow!("bg: no job control"));
        }
        let id = parse_job_id(self.job.as_deref());
        let pgid = find_job(shell, id)
            .map(|j| j.pgid)
            .ok_or_else(|| anyhow!("bg: no such job: {id}"))?;
        kill_process_group(pgid, Signal::CONT)?;
        if let Some(job) = shell.jobs.by_pgid_mut(pgid) {
            job.state = JobState::Running;
        }
        eprintln!("[{id}]+ {pgid} &");
        Ok(ExitStatus::SUCCESS)
    }
}
