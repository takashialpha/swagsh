use std::ffi::CString;
use std::fs::OpenOptions;
use std::os::fd::{BorrowedFd, IntoRawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::RawFd;
use std::path::PathBuf;

use anyhow::{Result, anyhow, bail};
use nix::sys::signal::{SigHandler, SigSet, SigmaskHow, Signal, signal, sigprocmask};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::{ForkResult, Pid, execvp, fork, getpid, pipe, setpgid, tcsetpgrp};

use crate::ast::{
    AndOrList, AndOrOp, CaseClause, Command, ForClause, GroupCmd, IfClause, Pipeline, Program,
    Redirect, RedirectKind, SimpleCmd, WhileClause, Word,
};
use crate::env::Env;

// ---------------------------------------------------------------------------
// Helpers — nix 0.31 API shims
// ---------------------------------------------------------------------------

#[inline]
fn tcsetpgrp_stdin(pgid: Pid) -> nix::Result<()> {
    // SAFETY: fd 0 is always valid for the lifetime of the shell process.
    let borrowed = unsafe { BorrowedFd::borrow_raw(0) };
    tcsetpgrp(borrowed, pgid)
}

#[inline]
fn dup2_raw(oldfd: RawFd, newfd: RawFd) -> nix::Result<()> {
    // SAFETY: raw fd integers, same contract as POSIX dup2.
    let ret = unsafe { libc::dup2(oldfd, newfd) };
    if ret == -1 {
        Err(nix::errno::Errno::last())
    } else {
        Ok(())
    }
}

#[inline]
fn close_raw(fd: RawFd) -> nix::Result<()> {
    let ret = unsafe { libc::close(fd) };
    if ret == -1 {
        Err(nix::errno::Errno::last())
    } else {
        Ok(())
    }
}

fn raw_pipe() -> nix::Result<(RawFd, RawFd)> {
    let (r, w) = pipe()?;
    Ok((r.into_raw_fd(), w.into_raw_fd()))
}

fn read_raw(fd: RawFd, buf: &mut [u8]) -> nix::Result<usize> {
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    nix::unistd::read(borrowed, buf)
}

fn write_raw(fd: RawFd, buf: &[u8]) -> nix::Result<usize> {
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    nix::unistd::write(borrowed, buf)
}

fn open_write(path: &std::path::Path, append: bool) -> Result<RawFd> {
    let mut opts = OpenOptions::new();
    opts.write(true).create(true);
    if append {
        opts.append(true);
    } else {
        opts.truncate(true);
    }
    opts.mode(0o644);
    let f = opts.open(path)?;
    Ok(f.into_raw_fd())
}

fn open_read(path: &std::path::Path) -> Result<RawFd> {
    let f = OpenOptions::new().read(true).open(path)?;
    Ok(f.into_raw_fd())
}

fn dup_save(fd: RawFd) -> nix::Result<RawFd> {
    let ret = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 10) };
    if ret == -1 {
        Err(nix::errno::Errno::last())
    } else {
        Ok(ret)
    }
}

// ---------------------------------------------------------------------------
// Exit status newtype
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitStatus(pub i32);

impl ExitStatus {
    pub const SUCCESS: Self = Self(0);
    pub const FAILURE: Self = Self(1);

    #[inline]
    pub fn is_success(self) -> bool {
        self.0 == 0
    }
}

// ---------------------------------------------------------------------------
// Job table
// ---------------------------------------------------------------------------

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
    fn add(&mut self, pgid: Pid, pids: Vec<Pid>, command: String) -> usize {
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

    fn remove(&mut self, id: usize) {
        self.jobs.retain(|j| j.id != id);
    }

    fn by_pgid_mut(&mut self, pgid: Pid) -> Option<&mut Job> {
        self.jobs.iter_mut().find(|j| j.pgid == pgid)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Job> {
        self.jobs.iter()
    }

    pub fn reap_nonblocking(&mut self) {
        loop {
            match waitpid(
                Pid::from_raw(-1),
                Some(WaitPidFlag::WNOHANG | WaitPidFlag::WUNTRACED),
            ) {
                Ok(WaitStatus::Exited(pid, code)) => self.mark_pid_done(pid, ExitStatus(code)),
                Ok(WaitStatus::Signaled(pid, sig, _)) => {
                    self.mark_pid_done(pid, ExitStatus(128 + sig as i32))
                }
                Ok(WaitStatus::Stopped(pid, _)) => self.mark_pid_stopped(pid),
                _ => break,
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

// ---------------------------------------------------------------------------
// Builtin dispatch — static sorted table, O(log n) binary search.
// Zero heap allocation, zero hashing, fits in a single cache line.
// INVARIANT: entries must remain sorted by name for binary_search to work.
// ---------------------------------------------------------------------------

type BuiltinFn = fn(&mut Executor, &[&str]) -> Result<ExitStatus>;

static BUILTINS: &[(&str, BuiltinFn)] = &[
    (".", builtin_source),
    (":", builtin_colon),
    ("bg", builtin_bg),
    ("break", builtin_break),
    ("cd", builtin_cd),
    ("continue", builtin_continue),
    ("echo", builtin_echo),
    ("exec", builtin_exec),
    ("exit", builtin_exit),
    ("export", builtin_export),
    ("false", builtin_false),
    ("fg", builtin_fg),
    ("jobs", builtin_jobs),
    ("kill", builtin_kill),
    ("printf", builtin_printf),
    ("pwd", builtin_pwd),
    ("set", builtin_set),
    ("source", builtin_source),
    ("true", builtin_true),
    ("unset", builtin_unset),
];

/// O(log n) builtin lookup — no allocation, no hashing, ~5 comparisons max.
#[inline]
fn lookup_builtin(name: &str) -> Option<BuiltinFn> {
    BUILTINS
        .binary_search_by_key(&name, |&(k, _)| k)
        .ok()
        .map(|i| BUILTINS[i].1)
}

// ---------------------------------------------------------------------------
// Executor
// ---------------------------------------------------------------------------

pub struct Executor {
    pub env: Env,
    pub jobs: JobTable,
    pub shell_pgid: Pid,
    pub last_status: ExitStatus,
}

impl Executor {
    pub fn new(env: Env) -> Result<Self> {
        let shell_pgid = getpid();
        let _ = setpgid(shell_pgid, shell_pgid);
        let _ = tcsetpgrp_stdin(shell_pgid);

        unsafe {
            let _ = signal(Signal::SIGTTOU, SigHandler::SigIgn);
            let _ = signal(Signal::SIGTTIN, SigHandler::SigIgn);
            let _ = signal(Signal::SIGTSTP, SigHandler::SigIgn);
        }

        Ok(Self {
            env,
            jobs: JobTable::default(),
            shell_pgid,
            last_status: ExitStatus::SUCCESS,
        })
    }

    // ------------------------------------------------------------------
    // Program entry
    // ------------------------------------------------------------------

    pub fn run_program(&mut self, program: &Program) -> Result<ExitStatus> {
        let mut status = ExitStatus::SUCCESS;
        for aol in &program.body {
            status = self.run_and_or(aol)?;
        }
        Ok(status)
    }

    // ------------------------------------------------------------------
    // And-or list
    // ------------------------------------------------------------------

    fn run_and_or(&mut self, aol: &AndOrList) -> Result<ExitStatus> {
        let mut status = ExitStatus::SUCCESS;

        for (i, item) in aol.items.iter().enumerate() {
            let is_last = i == aol.items.len() - 1;

            if aol.is_async && is_last {
                status = self.run_pipeline_async(&item.command)?;
            } else {
                status = self.run_pipeline(&item.command)?;
            }

            match item.op {
                None => {}
                Some(AndOrOp::And) if !status.is_success() => break,
                Some(AndOrOp::Or) if status.is_success() => break,
                _ => {}
            }
        }

        self.last_status = status;
        Ok(status)
    }

    // ------------------------------------------------------------------
    // Pipeline — foreground
    // ------------------------------------------------------------------

    pub fn run_pipeline(&mut self, pipeline: &Pipeline) -> Result<ExitStatus> {
        let n = pipeline.commands.len();

        if n == 1 {
            let mut status = self.run_command(&pipeline.commands[0], None, None)?;
            if pipeline.negated {
                status = if status.is_success() {
                    ExitStatus::FAILURE
                } else {
                    ExitStatus::SUCCESS
                };
            }
            return Ok(status);
        }

        let mut pgid: Option<Pid> = None;
        let mut pids = Vec::with_capacity(n);
        let mut prev_read: Option<RawFd> = None;

        for (idx, cmd) in pipeline.commands.iter().enumerate() {
            let is_last = idx == n - 1;
            let (pipe_read, pipe_write) = if !is_last {
                let (r, w) = raw_pipe()?;
                (Some(r), Some(w))
            } else {
                (None, None)
            };

            let pid = self.fork_command(cmd, prev_read, pipe_write, pgid)?;

            if let Some(existing_pgid) = pgid {
                let _ = setpgid(pid, existing_pgid);
            } else {
                pgid = Some(pid);
                let _ = setpgid(pid, pid);
            }

            pids.push(pid);
            if let Some(fd) = pipe_write {
                let _ = close_raw(fd);
            }
            if let Some(fd) = prev_read {
                let _ = close_raw(fd);
            }
            prev_read = pipe_read;
        }

        let pgid = pgid.unwrap();
        let _ = tcsetpgrp_stdin(pgid);

        let mut last_status = ExitStatus::SUCCESS;
        for pid in &pids {
            last_status = self.wait_for_pid(*pid)?;
        }

        let _ = tcsetpgrp_stdin(self.shell_pgid);

        if pipeline.negated {
            last_status = if last_status.is_success() {
                ExitStatus::FAILURE
            } else {
                ExitStatus::SUCCESS
            };
        }

        Ok(last_status)
    }

    // ------------------------------------------------------------------
    // Pipeline — background
    // ------------------------------------------------------------------

    fn run_pipeline_async(&mut self, pipeline: &Pipeline) -> Result<ExitStatus> {
        let n = pipeline.commands.len();
        let mut pgid: Option<Pid> = None;
        let mut pids = Vec::with_capacity(n);
        let mut prev_read: Option<RawFd> = None;
        let cmd_str = format!("{n} commands");

        for (idx, cmd) in pipeline.commands.iter().enumerate() {
            let is_last = idx == n - 1;
            let (pipe_read, pipe_write) = if !is_last {
                let (r, w) = raw_pipe()?;
                (Some(r), Some(w))
            } else {
                (None, None)
            };

            let pid = self.fork_command(cmd, prev_read, pipe_write, pgid)?;

            if let Some(existing_pgid) = pgid {
                let _ = setpgid(pid, existing_pgid);
            } else {
                pgid = Some(pid);
                let _ = setpgid(pid, pid);
            }

            pids.push(pid);
            if let Some(fd) = pipe_write {
                let _ = close_raw(fd);
            }
            if let Some(fd) = prev_read {
                let _ = close_raw(fd);
            }
            prev_read = pipe_read;
        }

        let pgid = pgid.unwrap();
        let job_id = self.jobs.add(pgid, pids, cmd_str);
        eprintln!("[{job_id}] {pgid}");

        Ok(ExitStatus::SUCCESS)
    }

    // ------------------------------------------------------------------
    // Fork a single pipeline stage
    // ------------------------------------------------------------------

    fn fork_command(
        &mut self,
        cmd: &Command,
        stdin_override: Option<RawFd>,
        stdout_override: Option<RawFd>,
        pgid: Option<Pid>,
    ) -> Result<Pid> {
        match unsafe { fork()? } {
            ForkResult::Child => {
                unsafe {
                    let _ = signal(Signal::SIGTTOU, SigHandler::SigDfl);
                    let _ = signal(Signal::SIGTTIN, SigHandler::SigDfl);
                    let _ = signal(Signal::SIGTSTP, SigHandler::SigDfl);
                    let _ = signal(Signal::SIGINT, SigHandler::SigDfl);
                    let _ = signal(Signal::SIGQUIT, SigHandler::SigDfl);
                }
                let _ = sigprocmask(SigmaskHow::SIG_SETMASK, Some(&SigSet::empty()), None);

                let my_pid = getpid();
                let group = pgid.unwrap_or(my_pid);
                let _ = setpgid(my_pid, group);

                if let Some(fd) = stdin_override {
                    let _ = dup2_raw(fd, 0);
                    let _ = close_raw(fd);
                }
                if let Some(fd) = stdout_override {
                    let _ = dup2_raw(fd, 1);
                    let _ = close_raw(fd);
                }

                let status = self.exec_command_in_child(cmd);
                std::process::exit(status);
            }
            ForkResult::Parent { child } => Ok(child),
        }
    }

    fn exec_command_in_child(&mut self, cmd: &Command) -> i32 {
        match cmd {
            Command::Simple(sc) => self.exec_simple_in_child(sc),
            Command::Group(g) => match self.run_group(g) {
                Ok(s) => s.0,
                Err(_) => 1,
            },
            other => match self.run_command(other, None, None) {
                Ok(s) => s.0,
                Err(_) => 1,
            },
        }
    }

    fn exec_simple_in_child(&mut self, sc: &SimpleCmd) -> i32 {
        if let Err(e) = self.apply_redirects(&sc.redirects) {
            eprintln!("swagsh: {e}");
            return 1;
        }
        let words = match self.expand_words(&sc.words) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("swagsh: {e}");
                return 1;
            }
        };
        if words.is_empty() {
            return 0;
        }

        // Strip and apply leading assignments (we are in a child — all
        // assignments are permanent for this process).
        let assign_count = words.iter().take_while(|w| is_assignment(w)).count();
        let (assignments, cmd_words) = words.split_at(assign_count);
        for a in assignments {
            let (k, v) = a.split_once('=').unwrap();
            self.env.export(k, v);
        }
        if cmd_words.is_empty() {
            return 0;
        }

        let name = &cmd_words[0];
        let args: Vec<&str> = cmd_words.iter().map(|s| s.as_str()).collect();

        if let Some(f) = lookup_builtin(name.as_str()) {
            return match f(self, &args[1..]) {
                Ok(s) => s.0,
                Err(e) => {
                    eprintln!("swagsh: {e}");
                    1
                }
            };
        }

        match self.do_exec(cmd_words) {
            Ok(_) => unreachable!(),
            Err(e) => {
                eprintln!("swagsh: {e}");
                127
            }
        }
    }

    // ------------------------------------------------------------------
    // Command dispatch
    // ------------------------------------------------------------------

    fn run_command(
        &mut self,
        cmd: &Command,
        _stdin: Option<RawFd>,
        _stdout: Option<RawFd>,
    ) -> Result<ExitStatus> {
        match cmd {
            Command::Simple(sc) => self.run_simple(sc),
            Command::Pipeline(p) => self.run_pipeline(p),
            Command::If(ic) => self.run_if(ic),
            Command::For(fc) => self.run_for(fc),
            Command::While(wc) => self.run_while(wc),
            Command::Case(cc) => self.run_case(cc),
            Command::Group(gc) => self.run_group(gc),
            Command::FunctionDef(fd) => {
                self.env.define_function(fd.name.clone(), *fd.body.clone());
                Ok(ExitStatus::SUCCESS)
            }
        }
    }

    // ------------------------------------------------------------------
    // Simple command (parent)
    // ------------------------------------------------------------------

    fn run_simple(&mut self, sc: &SimpleCmd) -> Result<ExitStatus> {
        let words = self.expand_words(&sc.words)?;

        // Split leading NAME=VALUE assignments from the actual command.
        let assign_count = words.iter().take_while(|w| is_assignment(w)).count();
        let (assignments, cmd_words) = words.split_at(assign_count);

        // ── No command — pure assignment statement ──────────────────────────
        if cmd_words.is_empty() {
            let saved = self.save_fds(&[0, 1, 2])?;
            self.apply_redirects(&sc.redirects)?;
            self.restore_fds(saved)?;
            for assign in assignments {
                let (name, value) = assign.split_once('=').unwrap();
                self.env.set(name, value);
            }
            return Ok(ExitStatus::SUCCESS);
        }

        let name = cmd_words[0].clone();
        let args: Vec<String> = cmd_words[1..].to_vec();
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let special = is_special_builtin(&name);

        // ── Builtin ─────────────────────────────────────────────────────────
        if let Some(f) = lookup_builtin(name.as_str()) {
            let saved_vars: Vec<(String, Option<String>)> = if !special {
                assignments
                    .iter()
                    .map(|a| {
                        let (k, v) = a.split_once('=').unwrap();
                        let old = self.env.get(k);
                        self.env.set(k, v);
                        (k.to_owned(), old)
                    })
                    .collect()
            } else {
                for a in assignments {
                    let (k, v) = a.split_once('=').unwrap();
                    self.env.set(k, v);
                }
                vec![]
            };

            let saved_fds = self.save_fds(&[0, 1, 2])?;
            self.apply_redirects(&sc.redirects)?;
            let result = f(self, &arg_refs);
            self.restore_fds(saved_fds)?;

            for (k, old) in saved_vars {
                match old {
                    Some(v) => self.env.set(&k, v),
                    None => self.env.unset(&k),
                }
            }
            return result;
        }

        // ── Shell function ───────────────────────────────────────────────────
        if let Some(body) = self.env.get_function(&name).cloned() {
            let saved_vars: Vec<(String, Option<String>)> = assignments
                .iter()
                .map(|a| {
                    let (k, v) = a.split_once('=').unwrap();
                    let old = self.env.get(k);
                    self.env.set(k, v);
                    (k.to_owned(), old)
                })
                .collect();

            let old_args = self.env.positional_args().to_vec();
            self.env.set_positional_args(args);
            let saved_fds = self.save_fds(&[0, 1, 2])?;
            self.apply_redirects(&sc.redirects)?;
            let status = self.run_command(&body, None, None)?;
            self.restore_fds(saved_fds)?;
            self.env.set_positional_args(old_args);

            for (k, old) in saved_vars {
                match old {
                    Some(v) => self.env.set(&k, v),
                    None => self.env.unset(&k),
                }
            }
            return Ok(status);
        }

        // ── External command ─────────────────────────────────────────────────
        self.run_external_with_assignments(sc, cmd_words, assignments)
    }

    fn run_external_with_assignments(
        &mut self,
        sc: &SimpleCmd,
        words: &[String],
        assignments: &[String],
    ) -> Result<ExitStatus> {
        match unsafe { fork()? } {
            ForkResult::Child => {
                unsafe {
                    let _ = signal(Signal::SIGTTOU, SigHandler::SigDfl);
                    let _ = signal(Signal::SIGTTIN, SigHandler::SigDfl);
                    let _ = signal(Signal::SIGTSTP, SigHandler::SigDfl);
                    let _ = signal(Signal::SIGINT, SigHandler::SigDfl);
                    let _ = signal(Signal::SIGQUIT, SigHandler::SigDfl);
                }
                let _ = sigprocmask(SigmaskHow::SIG_SETMASK, Some(&SigSet::empty()), None);
                let my_pid = getpid();
                let _ = setpgid(my_pid, my_pid);
                // Export assignments into the child's environment.
                for a in assignments {
                    let (k, v) = a.split_once('=').unwrap();
                    self.env.export(k, v);
                }
                if let Err(e) = self.apply_redirects(&sc.redirects) {
                    eprintln!("swagsh: {e}");
                    std::process::exit(1);
                }
                match self.do_exec(words) {
                    Ok(_) => unreachable!(),
                    Err(e) => {
                        eprintln!("swagsh: {e}");
                        std::process::exit(127);
                    }
                }
            }
            ForkResult::Parent { child } => {
                let _ = setpgid(child, child);
                let _ = tcsetpgrp_stdin(child);
                let status = self.wait_for_pid(child)?;
                let _ = tcsetpgrp_stdin(self.shell_pgid);
                Ok(status)
            }
        }
    }

    // ------------------------------------------------------------------
    // exec(3)
    // ------------------------------------------------------------------

    fn do_exec(&self, words: &[String]) -> Result<std::convert::Infallible> {
        if words.is_empty() {
            bail!("exec: no command");
        }
        let name = CString::new(words[0].as_str()).map_err(|e| anyhow!(e))?;
        let argv: Vec<CString> = words
            .iter()
            .map(|w| CString::new(w.as_str()).map_err(|e| anyhow!(e)))
            .collect::<Result<_>>()?;
        execvp(&name, &argv).map_err(|e| anyhow!("{}: {}", words[0], e))?;
        unreachable!()
    }

    // ------------------------------------------------------------------
    // Compound commands
    // ------------------------------------------------------------------

    fn run_if(&mut self, ic: &IfClause) -> Result<ExitStatus> {
        if self.run_list(&ic.condition)?.is_success() {
            return self.run_list(&ic.then_body);
        }
        for (elif_cond, elif_body) in &ic.elif_clauses {
            if self.run_list(elif_cond)?.is_success() {
                return self.run_list(elif_body);
            }
        }
        if let Some(else_body) = &ic.else_body {
            return self.run_list(else_body);
        }
        Ok(ExitStatus::SUCCESS)
    }

    fn run_for(&mut self, fc: &ForClause) -> Result<ExitStatus> {
        let items: Vec<String> = fc
            .items
            .iter()
            .map(|w| self.expand_word(w))
            .collect::<Result<Vec<Vec<String>>>>()?
            .into_iter()
            .flatten()
            .collect();

        let mut status = ExitStatus::SUCCESS;
        for item in items {
            self.env.set(&fc.var, item);
            match self.run_list(&fc.body) {
                Ok(s) => status = s,
                Err(e) if is_break(&e) => {
                    status = ExitStatus::SUCCESS;
                    break;
                }
                Err(e) if is_continue(&e) => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(status)
    }

    fn run_while(&mut self, wc: &WhileClause) -> Result<ExitStatus> {
        let mut status = ExitStatus::SUCCESS;
        loop {
            let cond = self.run_list(&wc.condition)?;
            let should_run = if wc.until {
                !cond.is_success()
            } else {
                cond.is_success()
            };
            if !should_run {
                break;
            }
            match self.run_list(&wc.body) {
                Ok(s) => status = s,
                Err(e) if is_break(&e) => {
                    status = ExitStatus::SUCCESS;
                    break;
                }
                Err(e) if is_continue(&e) => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(status)
    }

    fn run_case(&mut self, cc: &CaseClause) -> Result<ExitStatus> {
        let word = self
            .expand_word(&cc.word)?
            .into_iter()
            .next()
            .unwrap_or_default();
        for arm in &cc.arms {
            for pattern in &arm.patterns {
                let pat = self
                    .expand_word(pattern)?
                    .into_iter()
                    .next()
                    .unwrap_or_default();
                if glob_match(&pat, &word) {
                    return self.run_list(&arm.body);
                }
            }
        }
        Ok(ExitStatus::SUCCESS)
    }

    fn run_group(&mut self, gc: &GroupCmd) -> Result<ExitStatus> {
        if gc.subshell {
            match unsafe { fork()? } {
                ForkResult::Child => {
                    let status = self.run_list(&gc.body).unwrap_or(ExitStatus::FAILURE);
                    std::process::exit(status.0);
                }
                ForkResult::Parent { child } => return self.wait_for_pid(child),
            }
        }
        self.run_list(&gc.body)
    }

    fn run_list(&mut self, list: &[AndOrList]) -> Result<ExitStatus> {
        let mut status = ExitStatus::SUCCESS;
        for aol in list {
            status = self.run_and_or(aol)?;
        }
        Ok(status)
    }

    // ------------------------------------------------------------------
    // Redirections
    // ------------------------------------------------------------------

    fn apply_redirects(&self, redirects: &[Redirect]) -> Result<()> {
        for r in redirects {
            match &r.kind {
                RedirectKind::Out => {
                    let path = self.word_to_path(&r.target)?;
                    let fd = open_write(&path, false)?;
                    dup2_raw(fd, r.fd)?;
                    close_raw(fd)?;
                }
                RedirectKind::Append => {
                    let path = self.word_to_path(&r.target)?;
                    let fd = open_write(&path, true)?;
                    dup2_raw(fd, r.fd)?;
                    close_raw(fd)?;
                }
                RedirectKind::In => {
                    let path = self.word_to_path(&r.target)?;
                    let fd = open_read(&path)?;
                    dup2_raw(fd, r.fd)?;
                    close_raw(fd)?;
                }
                RedirectKind::FdOut => {
                    if let Word::Literal(s) = &r.target {
                        let target_fd: RawFd = s.parse().map_err(|_| anyhow!("invalid fd: {s}"))?;
                        dup2_raw(target_fd, r.fd)?;
                    }
                }
                RedirectKind::Both => {
                    let path = self.word_to_path(&r.target)?;
                    let fd = open_write(&path, false)?;
                    dup2_raw(fd, 1)?;
                    dup2_raw(fd, 2)?;
                    close_raw(fd)?;
                }
                RedirectKind::HereString => {
                    let content = match &r.target {
                        Word::Literal(s) => format!("{s}\n"),
                        other => {
                            let expanded = self.expand_word(other)?;
                            format!("{}\n", expanded.join(" "))
                        }
                    };
                    let (read_fd, write_fd) = raw_pipe()?;
                    write_raw(write_fd, content.as_bytes())?;
                    close_raw(write_fd)?;
                    dup2_raw(read_fd, 0)?;
                    close_raw(read_fd)?;
                }
            }
        }
        Ok(())
    }

    fn word_to_path(&self, word: &Word) -> Result<PathBuf> {
        let s = self
            .expand_word(word)?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("empty redirect target"))?;
        Ok(PathBuf::from(s))
    }

    // ------------------------------------------------------------------
    // Fd save/restore for builtins
    // ------------------------------------------------------------------

    fn save_fds(&self, fds: &[RawFd]) -> Result<Vec<(RawFd, RawFd)>> {
        fds.iter()
            .map(|&fd| {
                let saved = dup_save(fd).map_err(|e| anyhow!(e))?;
                Ok((fd, saved))
            })
            .collect()
    }

    fn restore_fds(&self, saved: Vec<(RawFd, RawFd)>) -> Result<()> {
        for (original, saved_fd) in saved {
            dup2_raw(saved_fd, original).map_err(|e| anyhow!(e))?;
            close_raw(saved_fd).map_err(|e| anyhow!(e))?;
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Wait
    // ------------------------------------------------------------------

    fn wait_for_pid(&mut self, pid: Pid) -> Result<ExitStatus> {
        loop {
            match waitpid(pid, Some(WaitPidFlag::WUNTRACED)) {
                Ok(WaitStatus::Exited(_, code)) => return Ok(ExitStatus(code)),
                Ok(WaitStatus::Signaled(_, sig, _)) => return Ok(ExitStatus(128 + sig as i32)),
                Ok(WaitStatus::Stopped(stopped_pid, _)) => {
                    if let Some(job) = self.jobs.by_pgid_mut(stopped_pid) {
                        job.state = JobState::Stopped;
                        eprintln!("\n[{}]+ Stopped\t{}", job.id, job.command);
                    }
                    return Ok(ExitStatus(130));
                }
                Ok(_) => continue,
                Err(nix::errno::Errno::EINTR) => continue,
                Err(e) => return Err(anyhow!(e)),
            }
        }
    }

    // ------------------------------------------------------------------
    // Word expansion
    // ------------------------------------------------------------------

    pub fn expand_word(&self, word: &Word) -> Result<Vec<String>> {
        let raw = self.expand_word_to_string(word)?;
        if raw.contains('*') || raw.contains('?') || raw.contains('[') {
            let matches = glob_expand(&raw);
            if !matches.is_empty() {
                return Ok(matches);
            }
        }
        Ok(vec![raw])
    }

    fn expand_word_to_string(&self, word: &Word) -> Result<String> {
        match word {
            Word::Literal(s) => Ok(s.clone()),
            Word::Var(name) => Ok(self.expand_var(name)),
            Word::CmdSub(cmd) => self.expand_cmd_sub(cmd),
            Word::Compound(parts) => {
                let mut result = String::new();
                for part in parts {
                    result.push_str(&self.expand_word_to_string(part)?);
                }
                Ok(result)
            }
        }
    }

    fn expand_var(&self, name: &str) -> String {
        match name {
            "?" => self.last_status.0.to_string(),
            "$" => std::process::id().to_string(),
            "0" => std::env::args().next().unwrap_or_default(),
            n if n.chars().all(|c| c.is_ascii_digit()) => {
                let idx: usize = n.parse().unwrap_or(0);
                self.env
                    .positional_args()
                    .get(idx.saturating_sub(1))
                    .cloned()
                    .unwrap_or_default()
            }
            "@" | "*" => self.env.positional_args().join(" "),
            "#" => self.env.positional_args().len().to_string(),
            name => {
                if let Some((var, op, default)) = parse_param_op(name) {
                    let val = self.env.get(var).unwrap_or_default();
                    match op {
                        ":-" => {
                            if val.is_empty() {
                                default.to_owned()
                            } else {
                                val
                            }
                        }
                        ":+" => {
                            if val.is_empty() {
                                String::new()
                            } else {
                                default.to_owned()
                            }
                        }
                        ":?" => {
                            if val.is_empty() {
                                eprintln!(
                                    "swagsh: {var}: {}",
                                    if default.is_empty() {
                                        "parameter not set"
                                    } else {
                                        default
                                    }
                                );
                            }
                            val
                        }
                        ":=" => {
                            if val.is_empty() {
                                default.to_owned()
                            } else {
                                val
                            }
                        }
                        _ => val,
                    }
                } else {
                    self.env.get(name).unwrap_or_default()
                }
            }
        }
    }

    fn expand_cmd_sub(&self, cmd: &Command) -> Result<String> {
        let (read_fd, write_fd) = raw_pipe()?;
        match unsafe { fork()? } {
            ForkResult::Child => {
                close_raw(read_fd)?;
                dup2_raw(write_fd, 1)?;
                close_raw(write_fd)?;
                let mut child_exec =
                    Executor::new(self.env.clone()).expect("executor init in cmd sub");
                let status = child_exec
                    .run_command(cmd, None, None)
                    .unwrap_or(ExitStatus::FAILURE);
                std::process::exit(status.0);
            }
            ForkResult::Parent { child } => {
                close_raw(write_fd)?;
                let mut output = Vec::new();
                let mut buf = [0u8; 512];
                loop {
                    match read_raw(read_fd, &mut buf) {
                        Ok(0) => break,
                        Ok(n) => output.extend_from_slice(&buf[..n]),
                        Err(nix::errno::Errno::EINTR) => continue,
                        Err(e) => return Err(anyhow!(e)),
                    }
                }
                close_raw(read_fd)?;
                let _ = waitpid(child, None);
                let s = String::from_utf8_lossy(&output)
                    .trim_end_matches('\n')
                    .to_owned();
                Ok(s)
            }
        }
    }

    pub fn expand_words(&self, words: &[Word]) -> Result<Vec<String>> {
        let ifs = self.env.get("IFS").unwrap_or_else(|| " \t\n".to_owned());
        let mut result = Vec::with_capacity(words.len());
        for word in words {
            for s in self.expand_word(word)? {
                if s.contains(|c: char| ifs.contains(c)) {
                    result.extend(
                        s.split(|c: char| ifs.contains(c))
                            .filter(|f| !f.is_empty())
                            .map(String::from),
                    );
                } else {
                    result.push(s);
                }
            }
        }
        Ok(result)
    }
}

// ---------------------------------------------------------------------------
// Built-ins
// ---------------------------------------------------------------------------

fn builtin_colon(_exec: &mut Executor, _args: &[&str]) -> Result<ExitStatus> {
    Ok(ExitStatus::SUCCESS)
}

fn builtin_true(_exec: &mut Executor, _args: &[&str]) -> Result<ExitStatus> {
    Ok(ExitStatus::SUCCESS)
}

fn builtin_false(_exec: &mut Executor, _args: &[&str]) -> Result<ExitStatus> {
    Ok(ExitStatus::FAILURE)
}

fn builtin_break(_exec: &mut Executor, _args: &[&str]) -> Result<ExitStatus> {
    Err(anyhow::anyhow!("__break__"))
}

fn builtin_continue(_exec: &mut Executor, _args: &[&str]) -> Result<ExitStatus> {
    Err(anyhow::anyhow!("__continue__"))
}

fn builtin_cd(exec: &mut Executor, args: &[&str]) -> Result<ExitStatus> {
    let target = match args.first() {
        Some(&"-") => exec.env.get("OLDPWD").unwrap_or_else(|| "/".into()),
        Some(&path) => {
            if path.starts_with('~') {
                let home = exec.env.get("HOME").unwrap_or_default();
                path.replacen('~', &home, 1)
            } else {
                path.to_owned()
            }
        }
        None => exec.env.get("HOME").unwrap_or_else(|| "/".into()),
    };

    let old = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    std::env::set_current_dir(&target).map_err(|e| anyhow!("cd: {target}: {e}"))?;

    exec.env.export("OLDPWD", old);
    let new = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    exec.env.export("PWD", new);

    Ok(ExitStatus::SUCCESS)
}

fn builtin_pwd(_exec: &mut Executor, _args: &[&str]) -> Result<ExitStatus> {
    let cwd = std::env::current_dir().map_err(|e| anyhow!("pwd: {e}"))?;
    println!("{}", cwd.display());
    Ok(ExitStatus::SUCCESS)
}

fn builtin_echo(_exec: &mut Executor, args: &[&str]) -> Result<ExitStatus> {
    let mut interpret_escapes = false;
    let mut no_newline = false;
    let mut start = 0;
    for arg in args {
        match *arg {
            "-n" => {
                no_newline = true;
                start += 1;
            }
            "-e" => {
                interpret_escapes = true;
                start += 1;
            }
            "-E" => {
                interpret_escapes = false;
                start += 1;
            }
            _ => break,
        }
    }
    let output = args[start..].join(" ");
    let output = if interpret_escapes {
        unescape(&output)
    } else {
        output
    };
    if no_newline {
        print!("{output}");
    } else {
        println!("{output}");
    }
    Ok(ExitStatus::SUCCESS)
}

fn builtin_printf(_exec: &mut Executor, args: &[&str]) -> Result<ExitStatus> {
    if args.is_empty() {
        bail!("printf: missing format string");
    }
    let fmt = unescape(args[0]);
    let mut arg_iter = args[1..].iter();
    let mut output = String::new();
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            match chars.next() {
                Some('s') => output.push_str(arg_iter.next().copied().unwrap_or("")),
                Some('d') => {
                    let n: i64 = arg_iter.next().copied().unwrap_or("0").parse().unwrap_or(0);
                    output.push_str(&n.to_string());
                }
                Some('f') => {
                    let f: f64 = arg_iter
                        .next()
                        .copied()
                        .unwrap_or("0")
                        .parse()
                        .unwrap_or(0.0);
                    output.push_str(&format!("{f:.6}"));
                }
                Some('%') => output.push('%'),
                Some(other) => {
                    output.push('%');
                    output.push(other);
                }
                None => output.push('%'),
            }
        } else {
            output.push(c);
        }
    }
    print!("{output}");
    Ok(ExitStatus::SUCCESS)
}

fn builtin_export(exec: &mut Executor, args: &[&str]) -> Result<ExitStatus> {
    if args.is_empty() {
        for (k, v) in exec.env.exported() {
            println!("export {k}={v}");
        }
        return Ok(ExitStatus::SUCCESS);
    }
    for arg in args {
        if let Some((k, v)) = arg.split_once('=') {
            exec.env.export(k, v.to_owned());
        } else {
            exec.env.mark_exported(arg);
        }
    }
    Ok(ExitStatus::SUCCESS)
}

fn builtin_unset(exec: &mut Executor, args: &[&str]) -> Result<ExitStatus> {
    for arg in args {
        exec.env.unset(arg);
    }
    Ok(ExitStatus::SUCCESS)
}

fn builtin_set(exec: &mut Executor, args: &[&str]) -> Result<ExitStatus> {
    if args.is_empty() {
        for (k, v) in exec.env.all_vars() {
            println!("{k}={v}");
        }
        return Ok(ExitStatus::SUCCESS);
    }
    if args.first() == Some(&"--") {
        exec.env
            .set_positional_args(args[1..].iter().map(|s| s.to_string()).collect());
    }
    Ok(ExitStatus::SUCCESS)
}

fn builtin_source(exec: &mut Executor, args: &[&str]) -> Result<ExitStatus> {
    let path = args
        .first()
        .ok_or_else(|| anyhow!("source: filename required"))?;
    let src = std::fs::read_to_string(path).map_err(|e| anyhow!("source: {path}: {e}"))?;
    let program = crate::parser::parse(&src).map_err(|e| anyhow!("source: {path}: {e}"))?;
    exec.run_program(&program)
}

fn builtin_exec(exec: &mut Executor, args: &[&str]) -> Result<ExitStatus> {
    if args.is_empty() {
        return Ok(ExitStatus::SUCCESS);
    }
    let words: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    exec.do_exec(&words)?;
    unreachable!()
}

fn builtin_exit(_exec: &mut Executor, args: &[&str]) -> Result<ExitStatus> {
    let code: i32 = args.first().and_then(|s| s.parse().ok()).unwrap_or(0);
    std::process::exit(code);
}

fn builtin_jobs(exec: &mut Executor, _args: &[&str]) -> Result<ExitStatus> {
    exec.jobs.reap_nonblocking();
    let jobs: Vec<(usize, String, String)> = exec
        .jobs
        .iter()
        .map(|j| {
            let state = match &j.state {
                JobState::Running => "Running".into(),
                JobState::Stopped => "Stopped".into(),
                JobState::Done(s) => format!("Done({})", s.0),
            };
            (j.id, state, j.command.clone())
        })
        .collect();
    for (id, state, cmd) in jobs {
        println!("[{id}]  {state}\t{cmd}");
    }
    Ok(ExitStatus::SUCCESS)
}

fn builtin_fg(exec: &mut Executor, args: &[&str]) -> Result<ExitStatus> {
    let id: usize = args
        .first()
        .and_then(|s| s.trim_start_matches('%').parse().ok())
        .unwrap_or(1);
    let (pgid, cmd) = exec
        .jobs
        .iter()
        .find(|j| j.id == id)
        .map(|j| (j.pgid, j.command.clone()))
        .ok_or_else(|| anyhow!("fg: no such job: {id}"))?;

    eprintln!("{cmd}");
    nix::sys::signal::killpg(pgid, Signal::SIGCONT)?;
    let _ = tcsetpgrp_stdin(pgid);

    let status = loop {
        match waitpid(Pid::from_raw(-pgid.as_raw()), Some(WaitPidFlag::WUNTRACED)) {
            Ok(WaitStatus::Exited(_, code)) => break ExitStatus(code),
            Ok(WaitStatus::Signaled(_, sig, _)) => break ExitStatus(128 + sig as i32),
            Ok(WaitStatus::Stopped(..)) => {
                let _ = tcsetpgrp_stdin(exec.shell_pgid);
                break ExitStatus(130);
            }
            Ok(_) | Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => return Err(anyhow!(e)),
        }
    };

    let _ = tcsetpgrp_stdin(exec.shell_pgid);
    exec.jobs.remove(id);
    Ok(status)
}

fn builtin_bg(exec: &mut Executor, args: &[&str]) -> Result<ExitStatus> {
    let id: usize = args
        .first()
        .and_then(|s| s.trim_start_matches('%').parse().ok())
        .unwrap_or(1);
    let pgid = exec
        .jobs
        .iter()
        .find(|j| j.id == id)
        .map(|j| j.pgid)
        .ok_or_else(|| anyhow!("bg: no such job: {id}"))?;
    nix::sys::signal::killpg(pgid, Signal::SIGCONT)?;
    eprintln!("[{id}]+ {pgid} &");
    Ok(ExitStatus::SUCCESS)
}

fn builtin_kill(exec: &mut Executor, args: &[&str]) -> Result<ExitStatus> {
    if args.is_empty() {
        bail!("kill: usage: kill [-signal] pid|%job");
    }

    let mut sig = Signal::SIGTERM;
    let mut targets = args;

    if args[0].starts_with('-') {
        let sig_str = args[0].trim_start_matches('-');
        sig = sig_str
            .parse::<i32>()
            .ok()
            .and_then(|n| Signal::try_from(n).ok())
            .or_else(|| parse_signal_name(sig_str))
            .ok_or_else(|| anyhow!("kill: invalid signal: {sig_str}"))?;
        targets = &args[1..];
    }

    for target in targets {
        let pid = if let Some(id_str) = target.strip_prefix('%') {
            let id: usize = id_str
                .parse()
                .map_err(|_| anyhow!("kill: invalid job: {target}"))?;
            exec.jobs
                .iter()
                .find(|j| j.id == id)
                .map(|j| j.pgid)
                .ok_or_else(|| anyhow!("kill: no such job: {id}"))?
        } else {
            let raw: i32 = target
                .parse()
                .map_err(|_| anyhow!("kill: invalid pid: {target}"))?;
            Pid::from_raw(raw)
        };
        nix::sys::signal::kill(pid, sig)?;
    }

    Ok(ExitStatus::SUCCESS)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    glob_match_inner(&p, &t)
}

fn glob_match_inner(p: &[char], t: &[char]) -> bool {
    match (p.first(), t.first()) {
        (None, None) => true,
        (None, _) => false,
        (Some(&'*'), _) => {
            glob_match_inner(&p[1..], t) || (!t.is_empty() && glob_match_inner(p, &t[1..]))
        }
        (_, None) => false,
        (Some(&'?'), _) => glob_match_inner(&p[1..], &t[1..]),
        (Some(pc), Some(tc)) => pc == tc && glob_match_inner(&p[1..], &t[1..]),
    }
}

fn glob_expand(pattern: &str) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(".") else {
        return vec![];
    };
    let mut results: Vec<String> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|name| glob_match(pattern, name))
        .collect();
    results.sort();
    results
}

fn parse_param_op(s: &str) -> Option<(&str, &str, &str)> {
    for op in &[":-", ":+", ":?", ":="] {
        if let Some(pos) = s.find(op) {
            return Some((&s[..pos], &s[pos..pos + 2], &s[pos + 2..]));
        }
    }
    None
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('\\') => out.push('\\'),
                Some('0') => out.push('\0'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn parse_signal_name(s: &str) -> Option<Signal> {
    match s.to_uppercase().as_str() {
        "HUP" | "SIGHUP" => Some(Signal::SIGHUP),
        "INT" | "SIGINT" => Some(Signal::SIGINT),
        "QUIT" | "SIGQUIT" => Some(Signal::SIGQUIT),
        "KILL" | "SIGKILL" => Some(Signal::SIGKILL),
        "TERM" | "SIGTERM" => Some(Signal::SIGTERM),
        "STOP" | "SIGSTOP" => Some(Signal::SIGSTOP),
        "CONT" | "SIGCONT" => Some(Signal::SIGCONT),
        "TSTP" | "SIGTSTP" => Some(Signal::SIGTSTP),
        "USR1" | "SIGUSR1" => Some(Signal::SIGUSR1),
        "USR2" | "SIGUSR2" => Some(Signal::SIGUSR2),
        _ => None,
    }
}

/// Return true if `word` is a bare variable assignment `NAME=VALUE`
/// where NAME is a valid identifier (POSIX §2.9.1).
#[inline]
fn is_assignment(word: &str) -> bool {
    let Some(eq) = word.find('=') else {
        return false;
    };
    let name = &word[..eq];
    !name.is_empty()
        && name
            .chars()
            .next()
            .map(|c| c.is_ascii_alphabetic() || c == '_')
            .unwrap_or(false)
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// POSIX special builtins — assignments before these persist in the shell env.
#[inline]
fn is_special_builtin(name: &str) -> bool {
    matches!(
        name,
        "." | ":"
            | "break"
            | "continue"
            | "exec"
            | "exit"
            | "export"
            | "return"
            | "set"
            | "source"
            | "unset"
    )
}

fn is_break(e: &anyhow::Error) -> bool {
    e.to_string() == "__break__"
}
fn is_continue(e: &anyhow::Error) -> bool {
    e.to_string() == "__continue__"
}
