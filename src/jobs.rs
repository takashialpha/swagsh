use rustix::process::{Pid, WaitOptions, wait};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitStatus(pub i32);

impl ExitStatus {
    pub const SUCCESS: Self = Self(0);
    pub const FAILURE: Self = Self(1);

    #[inline]
    pub const fn is_success(self) -> bool {
        self.0 == 0
    }
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
    pub state: JobState,
    pub command: String,
}

#[derive(Debug, Default)]
pub struct JobTable {
    jobs: Vec<Job>,
    next_id: usize,
}

impl JobTable {
    pub fn add(&mut self, pgid: Pid, pids: Vec<Pid>, command: String) -> usize {
        self.next_id += 1;
        let id = self.next_id;
        self.jobs.push(Job {
            id,
            pgid,
            pids,
            state: JobState::Running,
            command,
        });
        id
    }

    pub fn remove(&mut self, id: usize) {
        self.jobs.retain(|j| j.id != id);
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
            } else if status.stopped() {
                self.mark_pid_stopped(pid);
            }
        }
    }

    fn mark_pid_done(&mut self, pid: Pid, status: ExitStatus) {
        for job in &mut self.jobs {
            if job.pids.contains(&pid) {
                job.state = JobState::Done(status);
            }
        }
    }

    fn mark_pid_stopped(&mut self, pid: Pid) {
        for job in &mut self.jobs {
            if job.pids.contains(&pid) {
                job.state = JobState::Stopped;
            }
        }
    }
}
