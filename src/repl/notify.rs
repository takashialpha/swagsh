//! `set -b`: announcing finished background jobs while the prompt waits.
//!
//! The line editor holds the main thread while it waits for input, so a
//! helper thread watches the running jobs and prints through rustyline's
//! external printer, which redraws the prompt and the typed text below the
//! notice. The thread lives only for one `readline` call: the shell never
//! forks while it exists, so no child inherits a lock held by it. It never
//! reaps either; it reads exit statuses and stops with `WNOWAIT` and leaves
//! the reaping to the main loop. Exits wake it through pidfds, stops
//! through a pipe `SIGCHLD` writes to.

use std::sync::{Arc, mpsc};
use std::time::Duration;

use rustix::event::{PollFd, PollFlags, poll};
use rustix::fd::AsRawFd;
use rustix::fd::{AsFd, OwnedFd};
use rustix::io::Errno;
use rustix::pipe::{PipeFlags, pipe_with};
use rustix::process::{Pid, PidfdFlags, WaitId, WaitIdOptions, pidfd_open, waitid};
use rustyline::ExternalPrinter;

use super::LineEditor;

use crate::eval::Shell;
use crate::jobs::{ExitStatus, JobState, done_text, job_line};
use crate::signal::notify_sigchld;

/// A job being watched.
struct Watched {
    id: usize,
    mark: char,
    command: String,
    /// A pidfd for each of its processes still running, and whether it is
    /// the job's last process, whose status is the job's.
    pidfds: Vec<(OwnedFd, bool)>,
    /// Its processes still running, to look for stops among.
    pids: Vec<Pid>,
    /// Its stop was announced.
    stopped: bool,
    /// The job's status and killing signal, once its last process is done.
    status: Option<(ExitStatus, Option<i32>)>,
}

/// A running watcher thread.
pub struct Watcher {
    stop: OwnedFd,
    /// The read end of `stop`, held here too so that writing to `stop` after
    /// the thread is gone cannot raise `SIGPIPE`.
    stop_rx: Arc<OwnedFd>,
    /// Where `SIGCHLD` writes while the watcher runs.
    chld_tx: OwnedFd,
    announced: mpsc::Receiver<Announced>,
}

/// The jobs a watcher announced: finished, and stopped.
#[derive(Default)]
pub struct Announced {
    pub done: Vec<usize>,
    pub stopped: Vec<usize>,
}

impl Watcher {
    /// Starts watching the running jobs, if `set -b` is on and there are any.
    pub fn start(shell: &Shell, rl: &mut LineEditor) -> Option<Self> {
        if !shell.notify {
            return None;
        }
        let jobs: Vec<Watched> = shell
            .jobs
            .iter()
            .filter(|j| matches!(j.state, JobState::Running | JobState::Stopped))
            .filter_map(|j| {
                let mut pidfds = Vec::new();
                let mut pids = Vec::new();
                for (i, (&pid, status)) in j.pids.iter().zip(&j.statuses).enumerate() {
                    if status.is_none() {
                        let fd = pidfd_open(pid, PidfdFlags::empty()).ok()?;
                        pidfds.push((fd, i + 1 == j.pids.len()));
                        pids.push(pid);
                    }
                }
                let status = match j.statuses.last() {
                    Some(&Some(status)) => Some((status, None)),
                    _ => None,
                };
                Some(Watched {
                    id: j.id,
                    mark: shell.jobs.mark(j.id),
                    command: j.command.clone(),
                    pidfds,
                    pids,
                    stopped: j.state == JobState::Stopped,
                    status,
                })
            })
            .collect();
        if jobs.is_empty() {
            return None;
        }
        let printer = rl.create_external_printer().ok()?;
        // Stops wake no pidfd; `SIGCHLD` does, through this pipe.
        let (chld_rx, chld_tx) = pipe_with(PipeFlags::NONBLOCK | PipeFlags::CLOEXEC).ok()?;
        let (stop_rx, stop) = rustix::pipe::pipe().ok()?;
        let stop_rx = Arc::new(stop_rx);
        let thread_stop = Arc::clone(&stop_rx);
        let (tx, announced) = mpsc::channel();
        std::thread::Builder::new()
            .name("notify".to_owned())
            .spawn(move || {
                let _ = tx.send(watch(jobs, &thread_stop, &chld_rx, printer));
            })
            .ok()?;
        notify_sigchld(Some(chld_tx.as_raw_fd()));
        Some(Self {
            stop,
            stop_rx,
            chld_tx,
            announced,
        })
    }

    /// Stops the thread, returning the jobs it announced.
    ///
    /// The wait for it is bounded: should the printer block (rustyline
    /// queues one message at a time), the thread is left to finish on its
    /// own, and its jobs are announced again before the next prompt.
    pub fn finish(self) -> Announced {
        notify_sigchld(None);
        drop(self.chld_tx);
        let _ = rustix::io::write(&self.stop, b"x");
        drop(self.stop_rx);
        self.announced
            .recv_timeout(Duration::from_millis(200))
            .unwrap_or_default()
    }
}

/// Waits until every job in `jobs` has finished or `stop` is readable,
/// announcing each job as it finishes or stops; `chld` is readable after a
/// `SIGCHLD`.
fn watch(
    mut jobs: Vec<Watched>,
    stop: &OwnedFd,
    chld: &OwnedFd,
    mut printer: impl ExternalPrinter,
) -> Announced {
    let mut announced = Announced::default();
    while !jobs.is_empty() {
        let ready: Vec<bool> = {
            let mut fds = vec![
                PollFd::new(stop, PollFlags::IN),
                PollFd::new(chld, PollFlags::IN),
            ];
            fds.extend(
                jobs.iter()
                    .flat_map(|j| &j.pidfds)
                    .map(|(fd, _)| PollFd::new(fd, PollFlags::IN)),
            );
            match poll(&mut fds, None) {
                Ok(_) => {}
                Err(Errno::INTR) => continue,
                Err(_) => break,
            }
            fds.iter().map(|fd| !fd.revents().is_empty()).collect()
        };
        if ready[0] {
            break;
        }
        if ready[1] {
            let mut buf = [0u8; 64];
            while rustix::io::read(chld, &mut buf).is_ok_and(|n| n > 0) {}
            for job in jobs.iter_mut().filter(|j| !j.stopped) {
                if job.pids.iter().any(|&pid| is_stopped(pid)) {
                    let line = job_line(job.id, job.mark, "Stopped", &job.command);
                    if printer.print(line).is_ok() {
                        job.stopped = true;
                        announced.stopped.push(job.id);
                    }
                }
            }
        }
        let mut ready = ready[2..].iter();
        for job in &mut jobs {
            job.pidfds.retain(|(fd, last)| {
                if !ready.next().copied().unwrap_or(false) {
                    return true;
                }
                if *last {
                    job.status = exit_status(fd);
                }
                false
            });
        }
        jobs.retain(|job| {
            if !job.pidfds.is_empty() {
                return true;
            }
            let (status, signal) = job.status.unwrap_or((ExitStatus::SUCCESS, None));
            let line = job_line(job.id, job.mark, &done_text(status, signal), &job.command);
            if printer.print(line).is_ok() {
                announced.done.push(job.id);
            }
            false
        });
    }
    announced
}

/// Whether `pid` has stopped, leaving the stop to be reaped.
fn is_stopped(pid: Pid) -> bool {
    let options = WaitIdOptions::STOPPED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG;
    waitid(WaitId::Pid(pid), options).is_ok_and(|s| s.is_some_and(|s| s.stopped()))
}

/// The exit status of the exited process behind `pidfd`, leaving it to be
/// reaped.
fn exit_status(pidfd: &OwnedFd) -> Option<(ExitStatus, Option<i32>)> {
    let options = WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG;
    let status = waitid(WaitId::PidFd(pidfd.as_fd()), options).ok()??;
    if let Some(sig) = status.terminating_signal() {
        return Some((ExitStatus(128 + sig), Some(sig)));
    }
    status
        .exit_status()
        .map(|code| Some((ExitStatus(code), None)))?
}
