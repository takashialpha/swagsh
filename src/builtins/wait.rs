use anyhow::Result;
use clap::Parser;
use rustix::process::Pid;

use crate::errfmt::emit;
use crate::eval::Shell;
use crate::jobs::{ExitStatus, JobState};

use super::Builtin;

/// Resolves a `wait` operand (`%N` or a bare pid) to a job-table id: unlike
/// `fg`/`bg`'s `%job`-only argument, `wait` also accepts a raw pid, and that
/// pid may be any stage of a pipeline job, not just its process-group leader.
fn find_job_by_pid_or_spec(shell: &Shell, spec: &str) -> Option<usize> {
    if let Some(id_str) = spec.strip_prefix('%') {
        let id: usize = id_str.parse().ok()?;
        return shell.jobs.iter().find(|j| j.id == id).map(|j| j.id);
    }
    let raw: i32 = spec.parse().ok()?;
    let pid = Pid::from_raw(raw)?;
    shell
        .jobs
        .iter()
        .find(|j| j.pgid == pid || j.pids.contains(&pid))
        .map(|j| j.id)
}

/// Blocks until job `id` terminates (or returns immediately if it already
/// has, per `reap_nonblocking`), removing it from the job table either way.
fn wait_one(shell: &mut Shell, id: usize) -> Option<ExitStatus> {
    let (pgid, already_done) = {
        let job = shell.jobs.iter().find(|j| j.id == id)?;
        (job.pgid, matches!(job.state, JobState::Done(_)))
    };
    let status = if already_done {
        match shell.jobs.iter().find(|j| j.id == id)?.state {
            JobState::Done(s) => s,
            JobState::Running | JobState::Stopped => unreachable!("checked above"),
        }
    } else {
        shell.wait_for_pid(pgid).ok()?
    };
    shell.jobs.remove(id);
    Some(status)
}

#[derive(Parser)]
#[command(
    name = "wait",
    about = "Wait for job completion and return exit status"
)]
pub struct WaitBuiltin {
    /// Wait for a single job (the first of IDS to finish, or the next job
    /// to finish if no IDS are given) instead of every one of them
    #[arg(short = 'n')]
    next: bool,
    /// Accepted for compatibility; this shell's `wait` always blocks until
    /// the target actually terminates, the behavior `-f` asks for
    #[arg(short = 'f')]
    force: bool,
    /// Assign the pid/job whose status is returned to VAR (only meaningful
    /// together with -n)
    #[arg(short = 'p')]
    pid_var: Option<String>,
    ids: Vec<String>,
}

impl Builtin for WaitBuiltin {
    fn run(self, shell: &mut Shell) -> Result<ExitStatus> {
        let _ = self.force;
        shell.jobs.reap_nonblocking();

        let mut targets: Vec<usize> = Vec::new();
        if self.ids.is_empty() {
            targets.extend(shell.jobs.iter().map(|j| j.id));
        } else {
            for spec in &self.ids {
                match find_job_by_pid_or_spec(shell, spec) {
                    Some(id) => targets.push(id),
                    None => {
                        emit(format!("wait: pid {spec} is not a child of this shell"));
                        return Ok(ExitStatus(127));
                    }
                }
            }
        }

        if self.next {
            if targets.is_empty() {
                // `-n` fails outright when there's nothing to wait for.
                return Ok(ExitStatus::FAILURE);
            }
            loop {
                shell.jobs.reap_nonblocking();
                let Some(job) = shell.jobs.iter().find(|j| targets.contains(&j.id)) else {
                    return Ok(ExitStatus::FAILURE);
                };
                if let JobState::Done(status) = job.state {
                    let (id, pgid) = (job.id, job.pgid);
                    shell.jobs.remove(id);
                    if let Some(var) = &self.pid_var {
                        shell.env.set(var, pgid.as_raw_nonzero().to_string());
                    }
                    return Ok(status);
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }

        if self.ids.is_empty() {
            // Plain `wait`: always returns 0 regardless of what any of the
            // waited-for children actually exited with.
            for id in targets {
                wait_one(shell, id);
            }
            return Ok(ExitStatus::SUCCESS);
        }

        let mut status = ExitStatus::SUCCESS;
        for id in targets {
            if let Some(s) = wait_one(shell, id) {
                status = s;
            }
        }
        Ok(status)
    }
}
