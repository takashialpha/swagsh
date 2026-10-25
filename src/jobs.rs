use rustix::process::{Pid, WaitOptions, WaitStatus, wait};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitStatus(pub i32);

impl ExitStatus {
    pub const SUCCESS: Self = Self(0);
    pub const FAILURE: Self = Self(1);

    #[inline]
    #[must_use]
    pub const fn is_success(self) -> bool {
        self.0 == 0
    }

    /// The status a `!`-negated pipeline reports: POSIX only distinguishes
    /// success (0) from failure, so any nonzero status negates to `SUCCESS`.
    #[inline]
    #[must_use]
    pub const fn negated(self) -> Self {
        if self.is_success() {
            Self::FAILURE
        } else {
            Self::SUCCESS
        }
    }
}

/// Decodes a `waitpid`/`waitpgid` status for the two cases that end a wait
/// loop: normal exit, or death by signal.
///
/// Death by signal is reported as `128 + signum`, the POSIX convention `$?`
/// follows. Returns `None` for a stopped process, since `wait_for_pid` and
/// `fg` each handle that case differently (job-table bookkeeping vs.
/// terminal ownership).
#[must_use]
pub fn decode_wait_status(status: WaitStatus) -> Option<ExitStatus> {
    status
        .exit_status()
        .map(ExitStatus)
        .or_else(|| status.terminating_signal().map(|sig| ExitStatus(128 + sig)))
}

/// How one wait for a child ended.
#[derive(Debug, Clone, Copy)]
pub enum Waited {
    Exited(Pid, ExitStatus),
    Stopped(Pid),
    /// A trapped signal arrived first; its trap is still pending.
    Trapped(i32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobState {
    Running,
    Stopped,
    Done(ExitStatus),
}

#[derive(Debug)]
pub struct Job {
    pub id: usize,
    pub pgid: Pid,
    pub pids: Vec<Pid>,
    /// The exit status of each of `pids` that has finished.
    pub statuses: Vec<Option<ExitStatus>>,
    pub state: JobState,
    /// The signal that killed the job's last process, if one did.
    pub signal: Option<i32>,
    /// Its completion was already announced while the shell sat at the
    /// prompt (`set -b`).
    pub notified: bool,
    /// It was announced as stopped since it last stopped.
    pub stop_announced: bool,
    pub command: String,
}

impl Job {
    /// The job's state as `jobs` and completion notices word it.
    #[must_use]
    pub fn state_text(&self) -> String {
        match self.state {
            JobState::Running => "Running".to_owned(),
            JobState::Stopped => "Stopped".to_owned(),
            JobState::Done(status) => done_text(status, self.signal),
        }
    }
}

/// How a finished job's state reads: `Done`, `Exit 3`, or the description
/// of the signal that killed it (`Terminated`).
#[must_use]
pub fn done_text(status: ExitStatus, signal: Option<i32>) -> String {
    match signal {
        Some(sig) => crate::sys::signal_description(sig),
        None if status.is_success() => "Done".to_owned(),
        None => format!("Exit {}", status.0),
    }
}

/// A job line laid out as bash does: `[1]+  Running                    cmd`.
#[must_use]
pub fn job_line(id: usize, mark: char, state: &str, command: &str) -> String {
    format!("[{id}]{mark}  {state:<27}{command}")
}

#[derive(Debug, Default)]
pub struct JobTable {
    jobs: Vec<Job>,
    /// Exit statuses of processes whose jobs were reported finished and
    /// removed, newest last, so `wait PID` can still return them.
    forgotten: Vec<(Pid, ExitStatus)>,
}

/// How many statuses of reported jobs `JobTable` keeps for `wait`.
const FORGOTTEN_MAX: usize = 64;

impl JobTable {
    pub fn add(&mut self, pgid: Pid, pids: Vec<Pid>, command: String) -> usize {
        let id = self.jobs.iter().map(|j| j.id).max().unwrap_or(0) + 1;
        self.jobs.push(Job {
            id,
            pgid,
            statuses: vec![None; pids.len()],
            pids,
            state: JobState::Running,
            signal: None,
            notified: false,
            stop_announced: false,
            command,
        });
        id
    }

    pub fn remove(&mut self, id: usize) {
        self.jobs.retain(|j| j.id != id);
    }

    /// Drops a foreground job once it has been waited for, unless it was
    /// stopped and so became a background job.
    pub fn remove_unless_stopped(&mut self, id: usize) {
        self.jobs
            .retain(|j| j.id != id || j.state == JobState::Stopped);
    }

    /// `+` for the current job (the newest), `-` for the previous one, and
    /// a space for the rest, as `jobs` marks them.
    #[must_use]
    pub fn mark(&self, id: usize) -> char {
        let newer = self.jobs.iter().filter(|j| j.id > id).count();
        match newer {
            0 => '+',
            1 => '-',
            _ => ' ',
        }
    }

    /// One `jobs` line for `job`, laid out as bash does:
    /// `[1]+  Running                    sleep 9 &`, and with `pid`, the
    /// process group id after the mark.
    #[must_use]
    pub fn describe(&self, job: &Job, pid: bool) -> String {
        let suffix = if job.state == JobState::Running {
            " &"
        } else {
            ""
        };
        let mark = self.mark(job.id);
        let state = job.state_text();
        let id = job.id;
        if pid {
            format!(
                "[{id}]{mark} {} {state:<27}{}{suffix}",
                job.pgid, job.command
            )
        } else {
            job_line(id, mark, &state, &format!("{}{suffix}", job.command))
        }
    }

    /// Removes every finished job, returning the lines that announce the
    /// ones not announced yet.
    pub fn take_finished(&mut self) -> Vec<String> {
        let lines = self
            .jobs
            .iter()
            .filter(|j| matches!(j.state, JobState::Done(_)) && !j.notified)
            .map(|j| self.describe(j, false))
            .collect();
        let (done, live) = std::mem::take(&mut self.jobs)
            .into_iter()
            .partition(|j| matches!(j.state, JobState::Done(_)));
        self.jobs = live;
        for job in done {
            let statuses = job.pids.iter().zip(&job.statuses);
            self.forgotten
                .extend(statuses.filter_map(|(&p, s)| s.map(|s| (p, s))));
        }
        let excess = self.forgotten.len().saturating_sub(FORGOTTEN_MAX);
        self.forgotten.drain(..excess);
        lines
    }

    /// The lines announcing the jobs that stopped since they were last
    /// announced, which marks them announced.
    pub fn take_stopped(&mut self) -> Vec<String> {
        let ids: Vec<usize> = self
            .jobs
            .iter()
            .filter(|j| j.state == JobState::Stopped && !j.stop_announced)
            .map(|j| j.id)
            .collect();
        let lines = ids
            .iter()
            .filter_map(|id| self.jobs.iter().find(|j| j.id == *id))
            .map(|j| self.describe(j, false))
            .collect();
        for job in &mut self.jobs {
            if ids.contains(&job.id) {
                job.stop_announced = true;
            }
        }
        lines
    }

    /// Records that job `id` was announced as stopped already.
    pub fn mark_stop_announced(&mut self, id: usize) {
        if let Some(job) = self.jobs.iter_mut().find(|j| j.id == id) {
            job.stop_announced = true;
        }
    }

    /// Records that job `id`'s completion has been announced already.
    pub fn mark_notified(&mut self, id: usize) {
        if let Some(job) = self.jobs.iter_mut().find(|j| j.id == id) {
            job.notified = true;
        }
    }

    /// The status of `pid` if its job was already reported and removed,
    /// handed out once.
    pub fn take_forgotten(&mut self, pid: Pid) -> Option<ExitStatus> {
        let i = self.forgotten.iter().position(|&(p, _)| p == pid)?;
        Some(self.forgotten.remove(i).1)
    }

    pub fn by_pgid_mut(&mut self, pgid: Pid) -> Option<&mut Job> {
        self.jobs.iter_mut().find(|j| j.pgid == pgid)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Job> {
        self.jobs.iter()
    }

    pub fn reap_nonblocking(&mut self) {
        while let Ok(Some((pid, status))) = wait(WaitOptions::NOHANG | WaitOptions::UNTRACED) {
            if let Some(code) = status.exit_status() {
                self.mark_pid_done(pid, ExitStatus(code));
            } else if let Some(sig) = status.terminating_signal() {
                self.mark_pid_done(pid, ExitStatus(128 + sig));
                if let Some(job) = self.jobs.iter_mut().find(|j| j.pids.last() == Some(&pid)) {
                    job.signal = Some(sig);
                }
            } else if status.stopped() {
                self.mark_pid_stopped(pid);
            }
        }
    }

    /// Records that `pid` exited. Its job is done once every process in it
    /// has, with the status of the last one (a pipeline's last stage).
    pub fn mark_pid_done(&mut self, pid: Pid, status: ExitStatus) {
        for job in &mut self.jobs {
            if let Some(i) = job.pids.iter().position(|&p| p == pid) {
                job.statuses[i] = Some(status);
                if let Some(Some(last)) = job.statuses.last()
                    && job.statuses.iter().all(Option::is_some)
                {
                    job.state = JobState::Done(*last);
                }
            }
        }
    }

    /// The job whose last process is `pid`.
    pub fn by_last_pid_mut(&mut self, pid: Pid) -> Option<&mut Job> {
        self.jobs.iter_mut().find(|j| j.pids.last() == Some(&pid))
    }

    /// Records that `pid` stopped, and so its job.
    pub fn mark_pid_stopped(&mut self, pid: Pid) {
        for job in &mut self.jobs {
            if job.pids.contains(&pid) {
                job.state = JobState::Stopped;
            }
        }
    }
}
