use anyhow::Result;
use clap::Parser;
use rustix::process::Pid;

use crate::errfmt::emit;
use crate::eval::Shell;
use crate::jobs::{ExitStatus, JobState, Waited};

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

/// How waiting for one job ended.
enum Outcome {
    Done(ExitStatus),
    /// A trapped signal arrived first.
    Trapped(i32),
    /// The job stopped, or there was nothing left to wait for.
    Gone,
}

/// Blocks until job `id` terminates, removing it from the job table.
fn wait_one(shell: &mut Shell, id: usize) -> Outcome {
    loop {
        let Some(job) = shell.jobs.iter().find(|j| j.id == id) else {
            return Outcome::Gone;
        };
        let pending = match job.state {
            JobState::Done(status) => {
                shell.jobs.remove(id);
                return Outcome::Done(status);
            }
            JobState::Stopped => return Outcome::Gone,
            JobState::Running => job
                .pids
                .iter()
                .zip(&job.statuses)
                .find(|(_, s)| s.is_none())
                .map(|(&p, _)| p),
        };
        let Some(pid) = pending else {
            return Outcome::Gone;
        };
        match shell.wait_or_trap(Some(pid)) {
            Ok(Waited::Exited(pid, status)) => shell.jobs.mark_pid_done(pid, status),
            Ok(Waited::Trapped(sig)) => return Outcome::Trapped(sig),
            Ok(Waited::Stopped(_)) | Err(_) => return Outcome::Gone,
        }
    }
}

/// `wait -n`: blocks until any job in `targets` terminates.
fn wait_next(shell: &mut Shell, targets: &[usize], pid_var: Option<&str>) -> ExitStatus {
    loop {
        shell.jobs.reap_nonblocking();
        let mut any = false;
        let mut done = None;
        for job in shell.jobs.iter().filter(|j| targets.contains(&j.id)) {
            any = true;
            if let JobState::Done(status) = job.state {
                done = Some((job.id, job.pgid, status));
                break;
            }
        }
        if let Some((id, pgid, status)) = done {
            shell.jobs.remove(id);
            if let Some(var) = pid_var {
                shell.env.set(var, pgid.as_raw_nonzero().to_string());
            }
            return status;
        }
        if !any {
            return ExitStatus(127);
        }
        match shell.wait_or_trap(None) {
            Ok(Waited::Exited(pid, status)) => shell.jobs.mark_pid_done(pid, status),
            Ok(Waited::Trapped(sig)) => return ExitStatus(128 + sig),
            Ok(Waited::Stopped(_)) => {}
            Err(_) => return ExitStatus(127),
        }
    }
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
        // The status of the last operand when its job was already reported.
        let mut last_forgotten = None;
        if self.ids.is_empty() {
            targets.extend(shell.jobs.iter().map(|j| j.id));
        } else {
            for spec in &self.ids {
                if let Some(id) = find_job_by_pid_or_spec(shell, spec) {
                    targets.push(id);
                } else if let Some(status) = spec
                    .parse()
                    .ok()
                    .and_then(Pid::from_raw)
                    .and_then(|pid| shell.jobs.take_forgotten(pid))
                {
                    if Some(spec) == self.ids.last() {
                        last_forgotten = Some(status);
                    }
                } else {
                    emit(format!("wait: pid {spec} is not a child of this shell"));
                    return Ok(ExitStatus(127));
                }
            }
        }

        if self.next {
            if targets.is_empty() {
                return Ok(ExitStatus(127));
            }
            return Ok(wait_next(shell, &targets, self.pid_var.as_deref()));
        }

        // A plain `wait` returns 0 whatever the jobs exited with; with
        // operands it returns the last one's status.
        let mut status = ExitStatus::SUCCESS;
        for id in targets {
            match wait_one(shell, id) {
                Outcome::Done(s) if !self.ids.is_empty() => status = s,
                Outcome::Trapped(sig) => return Ok(ExitStatus(128 + sig)),
                Outcome::Done(_) | Outcome::Gone => {}
            }
        }
        Ok(last_forgotten.unwrap_or(status))
    }
}
