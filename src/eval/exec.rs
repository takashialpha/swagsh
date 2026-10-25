use std::ffi::CString;
use std::path::PathBuf;

use anyhow::{Result, anyhow, bail};
use rustix::fd::RawFd;
use rustix::io::Errno;
use rustix::process::{Pid, WaitOptions, getpid, setpgid, wait, waitpid};

use rustix::termios::tcsetpgrp;

use crate::ast::{Command, Pipeline, Redirect, RedirectKind, SimpleCmd, TimeFormat, Word};
use crate::builtins::limits::minutes_seconds;
use crate::errfmt::{emit, strerror};
use crate::fd::{
    close_if_open, close_raw, dup2_raw, open_read, open_read_write, open_write,
    open_write_noclobber, raw_pipe, set_cloexec, write_raw,
};
use crate::jobs::{ExitStatus, JobState, Waited, decode_wait_status};
use crate::signal::{first_pending_trap, restore_child_signals};
use crate::sys::{Fork, cpu_times, execve, fork};

use super::{Resolved, Shell, catch_return, is_break, is_continue, is_return};

impl Shell {
    /// # Errors
    ///
    /// Returns an error if forking or waiting for any stage fails.
    pub fn run_pipeline(&mut self, pipeline: &Pipeline) -> Result<ExitStatus> {
        let Some(format) = pipeline.timed else {
            return self.run_pipeline_untimed(pipeline);
        };
        let start = std::time::Instant::now();
        let (own, children) = (cpu_times(false), cpu_times(true));
        let result = self.run_pipeline_untimed(pipeline);
        let real = start.elapsed();
        let (own_after, children_after) = (cpu_times(false), cpu_times(true));
        let user = own_after.user.saturating_sub(own.user)
            + children_after.user.saturating_sub(children.user);
        let system = own_after.system.saturating_sub(own.system)
            + children_after.system.saturating_sub(children.system);
        match format {
            TimeFormat::Default => eprintln!(
                "\nreal\t{}\nuser\t{}\nsys\t{}",
                minutes_seconds(real),
                minutes_seconds(user),
                minutes_seconds(system)
            ),
            TimeFormat::Posix => eprintln!(
                "real {:.2}\nuser {:.2}\nsys {:.2}",
                real.as_secs_f64(),
                user.as_secs_f64(),
                system.as_secs_f64()
            ),
        }
        result
    }

    fn run_pipeline_untimed(&mut self, pipeline: &Pipeline) -> Result<ExitStatus> {
        let n = pipeline.commands.len();

        if n == 1 {
            let mut status = self.run_command(&pipeline.commands[0])?;
            self.pipestatus.clear();
            self.pipestatus.push(status.0);
            if pipeline.negated {
                status = status.negated();
            }
            return Ok(status);
        }

        if self.shopt.lastpipe && !self.monitor {
            return self.run_pipeline_lastpipe(pipeline);
        }
        let (pgid, pids) = self.spawn_pipeline(pipeline)?;
        if self.monitor {
            let _ = tcsetpgrp(std::io::stdin(), pgid);
        }

        let job_id = if self.monitor {
            Some(
                self.jobs
                    .add(pgid, pids.clone(), describe_pipeline(pipeline)),
            )
        } else {
            None
        };

        let mut last_status = ExitStatus::SUCCESS;
        let mut statuses = Vec::with_capacity(pids.len());
        for child in &pids {
            last_status = self.wait_for_pid(*child)?;
            statuses.push(last_status.0);
        }
        self.pipestatus = statuses;

        if self.monitor {
            let _ = tcsetpgrp(std::io::stdin(), self.pgid);
            self.restore_terminal();
            if let Some(id) = job_id {
                self.jobs.remove_unless_stopped(id);
            }
        }

        if pipeline.negated {
            last_status = last_status.negated();
        }
        Ok(last_status)
    }

    /// # Errors
    ///
    /// Returns an error if forking any stage fails.
    pub fn run_pipeline_async(&mut self, pipeline: &Pipeline) -> Result<ExitStatus> {
        let label = describe_pipeline(pipeline);
        let (pgid, pids) = self.spawn_pipeline(pipeline)?;
        self.last_bg_pid = pids.last().copied();
        let job_id = self.jobs.add(pgid, pids, label);
        // A backgrounded job's `[N] PID` is only announced under job
        // control (monitor mode), which is on by default for interactive
        // shells and off for `-c`/script runs; match that instead of
        // always printing it.
        if self.monitor {
            eprintln!("[{job_id}] {pgid}");
        }
        Ok(ExitStatus::SUCCESS)
    }

    /// `{name}>file` and the like: opens the target on a new descriptor of
    /// 10 or more and sets `name` to its number; `{name}>&-` closes the one
    /// `name` holds.
    fn apply_var_redirect(&mut self, r: &Redirect, var: &str) -> Result<()> {
        let opened = match &r.kind {
            RedirectKind::Dup => {
                let target = self.expand_word_to_string(&r.target)?;
                if target == "-" {
                    let value = self.expand_var_in(var, false)?;
                    let fd = value
                        .parse::<RawFd>()
                        .map_err(|_| anyhow!("{var}: not a file descriptor"))?;
                    close_if_open(fd);
                    return Ok(());
                }
                let fd: RawFd = target
                    .parse()
                    .map_err(|_| anyhow!("invalid fd: {target}"))?;
                let new = crate::fd::dup_at_least(fd, 10)
                    .map_err(|e| anyhow!("{target}: {}", strerror(e)))?;
                self.assign_target(var, new.to_string())?;
                return Ok(());
            }
            RedirectKind::Out | RedirectKind::Clobber => {
                self.open_target(&r.target, |p| open_write(p, false))?
            }
            RedirectKind::Append => self.open_target(&r.target, |p| open_write(p, true))?,
            RedirectKind::In => self.open_target(&r.target, open_read)?,
            RedirectKind::ReadWrite => self.open_target(&r.target, open_read_write)?,
            _ => bail!("{var}: this redirection cannot pick its own descriptor"),
        };
        let new = crate::fd::dup_at_least(opened, 10);
        close_raw(opened);
        let new = new.map_err(|e| anyhow!("{var}: {}", strerror(e)))?;
        self.assign_target(var, new.to_string())?;
        Ok(())
    }

    /// `coproc name command`: starts `command` in the background, its input
    /// and output pipes from and to the shell, and sets `name` to the
    /// array of the shell's ends, `[0]` to read from and `[1]` to write to,
    /// and `name_PID` to its pid.
    pub(super) fn run_coproc(&mut self, name: &str, body: &Command) -> Result<ExitStatus> {
        let (to_read, to_write) = raw_pipe()?;
        let (from_read, from_write) = raw_pipe()?;
        let pid = self.fork_command(body, Some(to_read), Some(from_write), Some(to_write), None)?;
        close_raw(to_read);
        close_raw(from_write);
        // Kept for the shell's own redirections, and from no one else.
        set_cloexec(to_write);
        set_cloexec(from_read);
        let mut array = crate::env::Array::new(false);
        array.set(crate::env::Key::Index(0), from_read.to_string());
        array.set(crate::env::Key::Index(1), to_write.to_string());
        self.env.set_array(name, array)?;
        self.env
            .assign(&format!("{name}_PID"), pid.as_raw_nonzero().to_string())?;
        self.last_bg_pid = Some(pid);
        let job_id = self.jobs.add(pid, vec![pid], format!("coproc {name}"));
        if self.monitor {
            eprintln!("[{job_id}] {pid}");
        }
        Ok(ExitStatus::SUCCESS)
    }

    /// Forks every stage of `pipeline`, wiring each stage's stdout to the
    /// next stage's stdin via a pipe, and (under job control) placing them
    /// all in one process group. Returns the first stage's pid, which is
    /// that group's id under job control, and the pids of every forked stage,
    /// in pipeline order. Shared by `run_pipeline` (waits synchronously) and
    /// `run_pipeline_async` (registers a background job and returns).
    fn spawn_pipeline(&mut self, pipeline: &Pipeline) -> Result<(Pid, Vec<Pid>)> {
        let (pgid, pids, _) = self.spawn_stages(pipeline, pipeline.commands.len())?;
        Ok((pgid, pids))
    }

    /// Forks the first `count` stages of `pipeline`. With fewer than all of
    /// them, also returns the read end of the pipe the last one forked
    /// writes to, for the caller to run the rest with.
    fn spawn_stages(
        &mut self,
        pipeline: &Pipeline,
        count: usize,
    ) -> Result<(Pid, Vec<Pid>, Option<RawFd>)> {
        let n = pipeline.commands.len();
        let mut pgid: Option<Pid> = None;
        let mut pids = Vec::with_capacity(count);
        let mut prev_read: Option<RawFd> = None;

        for (idx, cmd) in pipeline.commands.iter().enumerate().take(count) {
            let is_last = idx == n - 1;
            let (pipe_read, pipe_write) = if is_last {
                (None, None)
            } else {
                let (r, w) = raw_pipe()?;
                (Some(r), Some(w))
            };

            let child = self.fork_command(cmd, prev_read, pipe_write, pipe_read, pgid)?;

            let group = *pgid.get_or_insert(child);
            if self.monitor {
                let _ = setpgid(Some(child), Some(group));
            }

            pids.push(child);
            if let Some(fd) = pipe_write {
                close_raw(fd);
            }
            if let Some(fd) = prev_read {
                close_raw(fd);
            }
            prev_read = pipe_read;
        }

        let Some(pgid) = pgid else {
            bail!("internal error: spawn_pipeline called with an empty pipeline");
        };
        Ok((pgid, pids, prev_read))
    }

    /// `shopt lastpipe`: the pipeline's last command runs in this shell, its
    /// input the pipe from the stages before it, so it can set variables.
    fn run_pipeline_lastpipe(&mut self, pipeline: &Pipeline) -> Result<ExitStatus> {
        let n = pipeline.commands.len();
        let (_, pids, read) = self.spawn_stages(pipeline, n - 1)?;
        let saved = crate::fd::save_fds(&[0])?;
        if let Some(read) = read {
            let _ = dup2_raw(read, 0);
            close_raw(read);
        }
        let result = self.run_command(&pipeline.commands[n - 1]);
        crate::fd::restore_fds(saved)?;
        let mut statuses = Vec::with_capacity(n);
        for child in &pids {
            statuses.push(self.wait_for_pid(*child)?.0);
        }
        let mut status = result?;
        statuses.push(status.0);
        self.pipestatus = statuses;
        if pipeline.negated {
            status = status.negated();
        }
        Ok(status)
    }

    /// Forks one pipeline stage.
    ///
    /// `unused_pipe_end` is the read end of the pipe this stage *writes* into.
    /// That pipe has to exist before the fork, so the child inherits both of
    /// its ends, and the child must drop the read end: nothing here reads from
    /// it, and while it stays open the pipe still has a reader. The kernel
    /// then never raises `SIGPIPE` when the next stage exits, so a writer that
    /// outlives its reader (`yes | head -1`, `seq 1 100000 | head -1`) fills
    /// the pipe and blocks on the write forever instead of dying.
    fn fork_command(
        &mut self,
        cmd: &Command,
        stdin_override: Option<RawFd>,
        stdout_override: Option<RawFd>,
        unused_pipe_end: Option<RawFd>,
        pgid: Option<Pid>,
    ) -> Result<Pid> {
        // SAFETY: fork rules: async-signal-safe code only in child until exec.
        match unsafe { fork()? } {
            Fork::Child => {
                // SAFETY: in child, before any allocations.
                unsafe { restore_child_signals(self.interactive || self.monitor) };
                if self.monitor {
                    let my_pid = getpid();
                    let _ = setpgid(Some(my_pid), Some(pgid.unwrap_or(my_pid)));
                }
                // Before the `dup2`s below, so a descriptor number being
                // reused as 0/1 cannot close the wrong thing.
                if let Some(fd) = unused_pipe_end {
                    close_raw(fd);
                }
                if let Some(fd) = stdin_override {
                    let _ = dup2_raw(fd, 0);
                    close_raw(fd);
                }
                if let Some(fd) = stdout_override {
                    let _ = dup2_raw(fd, 1);
                    close_raw(fd);
                }
                self.reset_traps_for_subshell();
                let status = self.exec_in_child(cmd);
                self.exit(status);
            }
            Fork::ParentOf(child) => Ok(child),
        }
    }

    fn exec_in_child(&mut self, cmd: &Command) -> i32 {
        match cmd {
            Command::Simple(sc) => self.exec_simple_in_child(sc),
            other => match self.run_command(other) {
                Ok(s) => s.0,
                Err(e) => {
                    if !is_break(&e) && !is_continue(&e) && !is_return(&e) {
                        emit(e);
                    }
                    1
                }
            },
        }
    }

    /// Runs a `SimpleCmd` to completion inside a forked pipeline-stage
    /// child, returning its exit code. Resolution (word expansion, alias
    /// substitution, builtin/function/external lookup) is shared with the
    /// top-level `Shell::run_simple` path via `resolve_simple`; this
    /// function only decides how to *run* what was resolved, since a
    /// disposable forked child runs things differently than the continuing
    /// shell process does: assignments are exported (not scoped/restored)
    /// and external commands are exec'd directly instead of forked again.
    fn exec_simple_in_child(&mut self, sc: &SimpleCmd) -> i32 {
        if let Err(e) = self.apply_redirects(&sc.redirects) {
            emit(e);
            return 1;
        }
        let (assignments, resolved) = match self.resolve_simple(sc) {
            Ok(r) => r,
            Err(e) => {
                emit(e);
                return 1;
            }
        };
        for a in &assignments {
            let (k, v) = super::split_assignment(a);
            self.env.export(k, v);
        }
        if self.xtrace {
            self.print_xtrace(&assignments, &resolved);
        }

        match resolved {
            Resolved::AssignOnly => 0,
            Resolved::Builtin(f, _, args) => {
                let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
                match f(self, &arg_refs) {
                    Ok(s) => s.0,
                    Err(e) => {
                        emit(e);
                        1
                    }
                }
            }
            Resolved::Function(_, body, args) => {
                self.env.set_positional_args(args);
                match catch_return(self.run_command(&body)) {
                    Ok(s) => s.0,
                    Err(e) => {
                        emit(e);
                        1
                    }
                }
            }
            Resolved::External(words) => match Self::do_exec(&words, None) {
                Ok(never) => match never {},
                Err(e) => {
                    emit(&e);
                    exec_failure_status(&e)
                }
            },
        }
    }

    /// `pub` rather than `pub(super)` (the norm for this `impl Shell` block)
    /// so the `command` builtin can run its resolved external command
    /// through the same fork/job-control path as every other external
    /// command, without duplicating it: `command` deliberately skips
    /// *function* lookup but still needs the exact same spawn/wait/
    /// foreground-pgid handling as an ordinary external command.
    ///
    /// # Errors
    ///
    /// Returns an error if forking or waiting for the child fails.
    pub fn run_external(
        &mut self,
        sc: &SimpleCmd,
        words: &[String],
        assignments: &[String],
    ) -> Result<ExitStatus> {
        let resolved = words.first().and_then(|name| self.find_command(name));
        // SAFETY: fork; async-signal-safe code only in child.
        match unsafe { fork()? } {
            Fork::Child => {
                // SAFETY: in child, before any allocations.
                unsafe { restore_child_signals(self.interactive || self.monitor) };
                if self.monitor {
                    let my_pid = getpid();
                    let _ = setpgid(Some(my_pid), Some(my_pid));
                }
                for a in assignments {
                    let (k, v) = super::split_assignment(a);
                    self.env.export(k, v);
                }
                if let Err(e) = self.apply_redirects(&sc.redirects) {
                    emit(e);
                    std::process::exit(1);
                }
                match Self::do_exec(words, resolved.as_deref()) {
                    Ok(never) => match never {},
                    Err(e) => {
                        emit(&e);
                        std::process::exit(exec_failure_status(&e));
                    }
                }
            }
            Fork::ParentOf(child) => {
                if self.monitor {
                    let _ = setpgid(Some(child), Some(child));
                    let _ = tcsetpgrp(std::io::stdin(), child);
                }
                let job_id = if self.monitor {
                    Some(self.jobs.add(child, vec![child], words.join(" ")))
                } else {
                    None
                };
                let status = self.wait_for_pid(child)?;
                if self.monitor {
                    let _ = tcsetpgrp(std::io::stdin(), self.pgid);
                    self.restore_terminal();
                    if let Some(id) = job_id {
                        self.jobs.remove_unless_stopped(id);
                    }
                }
                Ok(status)
            }
        }
    }

    /// # Errors
    ///
    /// Returns an error (never returns `Ok`) if `words` is empty or `execvp`
    /// fails to replace the process image.
    pub fn do_exec(words: &[String], resolved: Option<&str>) -> Result<std::convert::Infallible> {
        if words.is_empty() {
            bail!("exec: no command");
        }
        let argv: Vec<CString> = words
            .iter()
            .map(|w| CString::new(w.as_str()))
            .collect::<Result<_, _>>()?;
        let resolved = resolved.map(CString::new).transpose()?;
        let errno = execvp_path(&argv, None, false, resolved.as_ref());
        Err(ExecFailure::new(&words[0], errno).into())
    }

    /// Where `$PATH` finds the external command `name`, through the `hash`
    /// table: a hit is used while it still exists, and a fresh search is
    /// remembered there. `None` for a name containing `/`, or one not found.
    pub fn find_command(&mut self, name: &str) -> Option<String> {
        if name.contains('/') {
            return None;
        }
        if let Some(path) = self.env.hashed_command(name)
            && is_executable(path)
        {
            return Some(path.to_owned());
        }
        let path_var = self.env.get("PATH").unwrap_or_default();
        let found = path_var
            .split(':')
            .map(|dir| {
                if dir.is_empty() {
                    format!("./{name}")
                } else {
                    format!("{dir}/{name}")
                }
            })
            .find(|candidate| is_executable(candidate))?;
        self.env.hash_command(name, found.clone());
        Some(found)
    }

    /// The `exec` builtin's no-COMMAND form applies its redirections
    /// permanently to the current shell instead of scoping them to a single
    /// call, the one case where `exec` doesn't replace the process image at
    /// all. Called directly from `run_builtin` instead of going through the
    /// usual `with_redirects`-wrapped `BuiltinFn` dispatch, since that
    /// always restores the original fds once the call returns.
    pub(super) fn run_exec_builtin(
        &mut self,
        args: &[&str],
        redirects: &[Redirect],
    ) -> Result<ExitStatus> {
        use crate::builtins::cli::{ParsedArgs, parse_args};
        use crate::builtins::script::ExecBuiltin;

        let parsed = match parse_args::<ExecBuiltin>(args)? {
            ParsedArgs::Ok(a) => a,
            ParsedArgs::Help => return Ok(ExitStatus::SUCCESS),
            ParsedArgs::UsageError => return Ok(ExitStatus(2)),
        };

        if parsed.command_and_args.is_empty() {
            return match self.apply_redirects(redirects) {
                Ok(()) => Ok(ExitStatus::SUCCESS),
                Err(e) => {
                    emit(e);
                    Ok(ExitStatus::FAILURE)
                }
            };
        }
        if let Err(e) = self.apply_redirects(redirects) {
            emit(e);
            return Ok(ExitStatus::FAILURE);
        }

        let words = parsed.command_and_args;

        let display_name = match (&parsed.name, parsed.login) {
            (Some(name), true) => Some(format!("-{name}")),
            (Some(name), false) => Some(name.clone()),
            (None, true) => Some(format!("-{}", words[0])),
            (None, false) => None,
        };

        let exec_argv: Vec<CString> = words
            .iter()
            .map(|w| CString::new(w.as_str()))
            .collect::<Result<_, _>>()?;
        let display_cstring = display_name.map(CString::new).transpose()?;
        let errno = execvp_path(&exec_argv, display_cstring.as_ref(), parsed.clear_env, None);
        emit(format!("exec: {}: {}", words[0], strerror(errno)));
        // A non-interactive shell exits outright when `exec`'s COMMAND
        // can't be run (no `execfail` option here to opt out of that).
        if !self.interactive {
            self.exit(127);
        }
        Ok(ExitStatus(127))
    }

    /// # Errors
    ///
    /// Returns an error if the underlying `waitpid` syscall fails.
    pub fn wait_for_pid(&mut self, pid: Pid) -> Result<ExitStatus> {
        if let Some(i) = self.reaped.iter().position(|w| waited_pid(*w) == Some(pid)) {
            return Ok(match self.reaped.remove(i) {
                Waited::Exited(_, status) => status,
                Waited::Stopped(_) | Waited::Trapped(_) => ExitStatus(130),
            });
        }
        // `set -b`: wait for any child, to announce background jobs the
        // moment they finish rather than after this command.
        let any = self.notify
            && self.monitor
            && self
                .jobs
                .iter()
                .any(|j| j.state == JobState::Running && !j.pids.contains(&pid));
        loop {
            match self.wait_once(if any { None } else { Some(pid) })? {
                Some(w) if waited_pid(w).is_some_and(|p| p != pid) => self.background_event(w),
                Some(Waited::Exited(_, status)) => return Ok(status),
                Some(Waited::Stopped(_)) => return Ok(ExitStatus(130)),
                Some(Waited::Trapped(_)) | None => {}
            }
        }
    }

    /// A child other than the one being waited for changed state: announce
    /// it if it is a background job's, or keep it for whoever waits for it.
    fn background_event(&mut self, waited: Waited) {
        let Some(pid) = waited_pid(waited) else {
            return;
        };
        let Some(id) = self
            .jobs
            .iter()
            .find(|j| j.pids.contains(&pid))
            .map(|j| j.id)
        else {
            // Bounded: a process substitution's child, say, is never
            // waited for by pid.
            if self.reaped.len() >= 64 {
                self.reaped.remove(0);
            }
            self.reaped.push(waited);
            return;
        };
        match waited {
            Waited::Exited(_, status) => self.jobs.mark_pid_done(pid, status),
            Waited::Stopped(_) => self.jobs.mark_pid_stopped(pid),
            Waited::Trapped(_) => {}
        }
        let Some(job) = self.jobs.iter().find(|j| j.id == id) else {
            return;
        };
        match job.state {
            JobState::Done(_) => {
                eprintln!("\n{}", self.jobs.describe(job, false));
                self.jobs.mark_notified(id);
            }
            JobState::Stopped if !job.stop_announced => {
                eprintln!("\n{}", self.jobs.describe(job, false));
                self.jobs.mark_stop_announced(id);
            }
            _ => {}
        }
    }

    /// Waits for `pid`, or with `None` for any child, the way the `wait`
    /// builtin does: a trapped signal ends the wait early with
    /// `Waited::Trapped`, leaving the trap pending for the caller to run.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying `waitpid` syscall fails, `ECHILD`
    /// included.
    pub fn wait_or_trap(&mut self, pid: Option<Pid>) -> Result<Waited> {
        loop {
            if let Some(sig) = first_pending_trap() {
                return Ok(Waited::Trapped(sig));
            }
            if let Some(waited) = self.wait_once(pid)? {
                return Ok(waited);
            }
        }
    }

    /// One `waitpid` call. `None` means it was interrupted by a signal or
    /// reported nothing, and should be retried.
    fn wait_once(&mut self, pid: Option<Pid>) -> Result<Option<Waited>> {
        // `waitpid(None)` would mean only this shell's own process group;
        // `wait` is any child, background jobs' groups included.
        let waited = pid.map_or_else(
            || wait(WaitOptions::UNTRACED),
            |pid| waitpid(Some(pid), WaitOptions::UNTRACED),
        );
        match waited {
            Ok(Some((pid, status))) => {
                if let Some(exit) = decode_wait_status(status) {
                    if let Some(sig) = status.terminating_signal()
                        && let Some(job) = self.jobs.by_last_pid_mut(pid)
                    {
                        job.signal = Some(sig);
                    }
                    return Ok(Some(Waited::Exited(pid, exit)));
                }
                if status.stopped() {
                    if let Some(job) = self.jobs.by_pgid_mut(pid) {
                        job.state = JobState::Stopped;
                        job.stop_announced = true;
                    }
                    if let Some(job) = self.jobs.iter().find(|j| j.pgid == pid) {
                        eprintln!("\n{}", self.jobs.describe(job, false));
                    }
                    return Ok(Some(Waited::Stopped(pid)));
                }
                Ok(None)
            }
            Ok(None) => Ok(None),
            Err(e) if e == Errno::INTR => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub(super) fn apply_redirects(&mut self, redirects: &[Redirect]) -> Result<()> {
        for r in redirects {
            if let Some(var) = &r.fd_var {
                self.apply_var_redirect(r, var)?;
                continue;
            }
            match &r.kind {
                RedirectKind::Out if self.noclobber => {
                    let fd = self.open_target(&r.target, open_write_noclobber)?;
                    move_fd(fd, r.fd)?;
                }
                RedirectKind::Out | RedirectKind::Clobber => {
                    let fd = self.open_target(&r.target, |p| open_write(p, false))?;
                    move_fd(fd, r.fd)?;
                }
                RedirectKind::ReadWrite => {
                    let fd = self.open_target(&r.target, open_read_write)?;
                    move_fd(fd, r.fd)?;
                }
                RedirectKind::Append => {
                    let fd = self.open_target(&r.target, |p| open_write(p, true))?;
                    move_fd(fd, r.fd)?;
                }
                RedirectKind::In => {
                    let fd = self.open_target(&r.target, open_read)?;
                    move_fd(fd, r.fd)?;
                }
                RedirectKind::Dup => {
                    // Expanded as a word rather than matched against
                    // `Word::Literal`: `>&$fd` is an ordinary word, and the
                    // literal-only match silently dropped the redirect for
                    // anything else, leaving output on the original
                    // descriptor with no error at all.
                    let target = self.expand_word_to_string(&r.target)?;
                    if target == "-" {
                        // POSIX `>&-`/`<&-`: close the descriptor outright, and
                        // quietly do nothing when it was not open to begin
                        // with (see `close_if_open`).
                        close_if_open(r.fd);
                    } else {
                        let target_fd: RawFd = target
                            .parse()
                            .map_err(|_| anyhow!("invalid fd: {target}"))?;
                        dup2_raw(target_fd, r.fd)
                            .map_err(|e| anyhow!("{target}: {}", strerror(e)))?;
                    }
                }
                RedirectKind::Both => {
                    let fd = open_write(&self.word_to_path(&r.target)?, false)?;
                    dup2_raw(fd, 1)?;
                    dup2_raw(fd, 2)?;
                    if fd != 1 && fd != 2 {
                        close_raw(fd);
                    }
                }
                RedirectKind::HereDoc { raw_body, quoted } => {
                    let content = if *quoted {
                        let mut s = raw_body.clone();
                        if !s.ends_with('\n') {
                            s.push('\n');
                        }
                        s
                    } else {
                        self.expand_heredoc_body(raw_body)?
                    };
                    write_herestring(&content)?;
                }
                RedirectKind::HereString => {
                    // `<<<`'s operand is an ordinary word by the time it
                    // gets here (`parser::parse_redirect` already ran it
                    // through the normal word-decomposition path), so it
                    // just needs the normal word-expansion + a trailing
                    // newline (always appended, even if the word already
                    // ends with its own).
                    let mut content = self.expand_word_to_string(&r.target)?;
                    if !content.ends_with('\n') {
                        content.push('\n');
                    }
                    write_herestring(&content)?;
                }
            }
        }
        Ok(())
    }

    /// Opens a redirect's target file, naming it in any error.
    fn open_target(
        &mut self,
        word: &Word,
        open: impl FnOnce(&std::path::Path) -> Result<RawFd>,
    ) -> Result<RawFd> {
        let path = self.word_to_path(word)?;
        open(&path).map_err(|e| anyhow!("{}: {}", path.display(), strerror(e)))
    }

    fn word_to_path(&mut self, word: &Word) -> Result<PathBuf> {
        let s = self
            .expand_word(word)?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("empty redirect target"))?;
        Ok(PathBuf::from(s))
    }
}

fn write_herestring(content: &str) -> Result<()> {
    let (read_fd, write_fd) = raw_pipe()?;
    let bytes = content.as_bytes();
    if bytes.len() < 65536 {
        let mut written = 0;
        while written < bytes.len() {
            match write_raw(write_fd, &bytes[written..]) {
                Ok(n) => written += n,
                Err(_) => break,
            }
        }
    } else {
        // SAFETY: grandchild only writes to pipe and exits immediately.
        if matches!(unsafe { fork() }, Ok(Fork::Child)) {
            close_raw(read_fd);
            let mut written = 0;
            while written < bytes.len() {
                match write_raw(write_fd, &bytes[written..]) {
                    Ok(n) => written += n,
                    Err(_) => break,
                }
            }
            close_raw(write_fd);
            std::process::exit(0);
        }
    }
    close_raw(write_fd);
    dup2_raw(read_fd, 0)?;
    close_raw(read_fd);
    Ok(())
}

/// Best-effort reconstruction of a pipeline's source text for job-control
/// display (`[1]+  Running    sleep 5`, `jobs`, ...). The AST doesn't keep
/// the original source span, so this rebuilds something close to it from
/// the parsed words instead of the generic `"N commands"` placeholder that
/// gave every backgrounded job the same uninformative label regardless of
/// what it actually ran.
fn describe_pipeline(pipeline: &Pipeline) -> String {
    let body = pipeline
        .commands
        .iter()
        .map(describe_command)
        .collect::<Vec<_>>()
        .join(" | ");
    if pipeline.negated {
        format!("! {body}")
    } else {
        body
    }
}

/// A simple command as `BASH_COMMAND` shows it.
pub(super) fn describe_simple(sc: &SimpleCmd) -> String {
    sc.words
        .iter()
        .map(describe_word)
        .collect::<Vec<_>>()
        .join(" ")
}

fn describe_command(cmd: &Command) -> String {
    match cmd {
        Command::Simple(sc) => sc
            .words
            .iter()
            .map(describe_word)
            .collect::<Vec<_>>()
            .join(" "),
        Command::Pipeline(p) => describe_pipeline(p),
        Command::If(_) => "if ...".to_owned(),
        Command::For(_) | Command::ArithFor(_) => "for ...".to_owned(),
        Command::Select(_) => "select ...".to_owned(),
        Command::Coproc(name, _) => format!("coproc {name}"),
        Command::Arith(expr) => format!("(({expr}))"),
        Command::Cond(_) => "[[ ... ]]".to_owned(),
        Command::While(_) => "while ...".to_owned(),
        Command::Case(_) => "case ...".to_owned(),
        Command::Group(gc) if gc.subshell => "(...)".to_owned(),
        Command::Group(_) => "{ ... }".to_owned(),
        Command::FunctionDef(fd) => format!("{}()", fd.name),
        Command::Redirected(inner, _) => describe_command(inner),
    }
}

fn describe_word(word: &Word) -> String {
    match word {
        Word::Literal(s) => s.clone(),
        Word::Var(name) => format!("${name}"),
        Word::Arith(expr) => format!("$(({expr}))"),
        Word::CmdSub(_) => "$(...)".to_owned(),
        Word::ProcSub { output, .. } => if *output { ">(...)" } else { "<(...)" }.to_owned(),
        Word::Compound(parts) => parts.iter().map(describe_word).collect(),
        Word::Quoted(inner) => describe_word(inner),
        Word::Assign(assign) => format!("{}=...", assign.name),
    }
}

/// `display_argv0`, when given, is what the exec'd program sees as its own
/// `argv[0]` (`exec -a name`/`-l`) instead of whatever name/path was used to
/// locate it on disk; those are independent by design in every Unix exec
/// family. `empty_env` is `exec -c`: run with no inherited environment at
/// all rather than the shell's own.
fn execvp_path(
    argv: &[CString],
    display_argv0: Option<&CString>,
    empty_env: bool,
    resolved: Option<&CString>,
) -> Errno {
    let mut argv_ptrs: Vec<*const u8> = argv.iter().map(|s| s.as_ptr().cast::<u8>()).collect();
    argv_ptrs.push(std::ptr::null());
    if let Some(d) = display_argv0 {
        argv_ptrs[0] = d.as_ptr().cast::<u8>();
    }

    let env_cstrings: Vec<CString> = if empty_env {
        Vec::new()
    } else {
        std::env::vars_os()
            .filter_map(|(k, v)| {
                let mut kv = k.into_encoded_bytes();
                kv.push(b'=');
                kv.extend(v.into_encoded_bytes());
                CString::new(kv).ok()
            })
            .collect()
    };
    let mut envp_ptrs: Vec<*const u8> = env_cstrings
        .iter()
        .map(|s| s.as_ptr().cast::<u8>())
        .collect();
    envp_ptrs.push(std::ptr::null());

    if let Some(path) = resolved {
        // SAFETY: null-terminated arrays of valid CString data.
        return unsafe { execve(path, argv_ptrs.as_ptr(), envp_ptrs.as_ptr()) };
    }

    let name = &argv[0];
    let name_bytes = name.to_bytes();

    if name_bytes.contains(&b'/') {
        // SAFETY: null-terminated arrays of valid CString data.
        return unsafe { execve(name, argv_ptrs.as_ptr(), envp_ptrs.as_ptr()) };
    }

    let path_var =
        std::env::var_os("PATH").unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin".into());
    let mut last_err = Errno::NOENT;
    for dir in std::env::split_paths(&path_var) {
        let mut full = dir;
        full.push(std::str::from_utf8(name_bytes).unwrap_or(""));
        if let Ok(candidate) = CString::new(full.as_os_str().as_encoded_bytes()) {
            if display_argv0.is_none() {
                argv_ptrs[0] = candidate.as_ptr().cast::<u8>();
            }
            // SAFETY: null-terminated arrays of valid CString data.
            let err = unsafe { execve(&candidate, argv_ptrs.as_ptr(), envp_ptrs.as_ptr()) };
            if err != Errno::NOENT && err != Errno::NOTDIR {
                return err;
            }
            last_err = err;
        }
    }
    last_err
}

/// A regular file with an execute bit set.
fn is_executable(path: &str) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// A command that could not be executed, with the status POSIX gives it:
/// 127 when it was not found, 126 when it was found but cannot be run.
#[derive(Debug)]
struct ExecFailure {
    message: String,
    status: i32,
}

impl ExecFailure {
    fn new(name: &str, errno: Errno) -> Self {
        let (message, status) = if errno == Errno::NOENT {
            let what = if name.contains('/') {
                "No such file or directory"
            } else {
                "command not found"
            };
            (format!("{name}: {what}"), 127)
        } else {
            (format!("{name}: {}", strerror(errno)), 126)
        };
        Self { message, status }
    }
}

impl std::fmt::Display for ExecFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ExecFailure {}

fn exec_failure_status(e: &anyhow::Error) -> i32 {
    e.downcast_ref::<ExecFailure>().map_or(127, |f| f.status)
}

/// Moves a freshly opened descriptor onto the slot the redirection names,
/// dropping the original.
///
/// The `fd == target` guard is the whole point: the kernel hands out the
/// lowest free descriptor, so `exec 3>file` in a shell holding only 0-2 gets
/// exactly fd 3 back. `dup2(3, 3)` is a documented no-op, and the `close(3)`
/// that used to follow it then shut the redirection down again, so
/// `exec 3>file; echo x >&3` reported EBADF while fd 4 and up worked.
fn move_fd(fd: RawFd, target: RawFd) -> Result<()> {
    if fd == target {
        return Ok(());
    }
    dup2_raw(fd, target)?;
    close_raw(fd);
    Ok(())
}

/// The child a [`Waited`] is about, unless it is a trap.
const fn waited_pid(waited: Waited) -> Option<Pid> {
    match waited {
        Waited::Exited(pid, _) | Waited::Stopped(pid) => Some(pid),
        Waited::Trapped(_) => None,
    }
}
