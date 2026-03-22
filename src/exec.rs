use std::ffi::CString;
use std::fs::OpenOptions;
use std::os::fd::{BorrowedFd, IntoRawFd};
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
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
    let borrowed = unsafe { BorrowedFd::borrow_raw(0) };
    tcsetpgrp(borrowed, pgid)
}

#[inline]
fn dup2_raw(oldfd: RawFd, newfd: RawFd) -> nix::Result<()> {
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
    Ok(opts.open(path)?.into_raw_fd())
}

fn open_read(path: &std::path::Path) -> Result<RawFd> {
    Ok(OpenOptions::new().read(true).open(path)?.into_raw_fd())
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
// Exit status
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
// Builtin dispatch — static sorted table, O(log n) binary search
// INVARIANT: entries must stay sorted by name.
// ---------------------------------------------------------------------------

type BuiltinFn = fn(&mut Executor, &[&str]) -> Result<ExitStatus>;

pub static BUILTINS: &[(&str, BuiltinFn)] = &[
    // INVARIANT: must be in strict lexicographic (byte) order for binary_search.
    // Verify with: entries.is_sorted_by_key(|(k,_)| k)
    (".", builtin_source),
    (":", builtin_colon),
    ("[", builtin_bracket),
    ("[[", builtin_double_bracket),
    ("alias", builtin_alias),
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
    ("read", builtin_read),
    ("set", builtin_set),
    ("shift", builtin_shift),
    ("source", builtin_source),
    ("test", builtin_test),
    ("true", builtin_true),
    ("unalias", builtin_unalias),
    ("unset", builtin_unset),
];

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
    /// True when running interactively — controls job control and alias expansion.
    pub interactive: bool,
}

impl Executor {
    pub fn new(env: Env, interactive: bool) -> Result<Self> {
        let shell_pgid = getpid();

        if interactive {
            let _ = setpgid(shell_pgid, shell_pgid);
            let _ = tcsetpgrp_stdin(shell_pgid);
            unsafe {
                let _ = signal(Signal::SIGTTOU, SigHandler::SigIgn);
                let _ = signal(Signal::SIGTTIN, SigHandler::SigIgn);
                let _ = signal(Signal::SIGTSTP, SigHandler::SigIgn);
            }
        }

        Ok(Self {
            env,
            jobs: JobTable::default(),
            shell_pgid,
            last_status: ExitStatus::SUCCESS,
            interactive,
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
            status = if aol.is_async && is_last {
                self.run_pipeline_async(&item.command)?
            } else {
                self.run_pipeline(&item.command)?
            };
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

            if let Some(existing) = pgid {
                let _ = setpgid(pid, existing);
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
        if self.interactive {
            let _ = tcsetpgrp_stdin(pgid);
        }

        let mut last_status = ExitStatus::SUCCESS;
        for pid in &pids {
            last_status = self.wait_for_pid(*pid)?;
        }

        if self.interactive {
            let _ = tcsetpgrp_stdin(self.shell_pgid);
        }

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

            if let Some(existing) = pgid {
                let _ = setpgid(pid, existing);
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

        let assign_count = words.iter().take_while(|w| is_assignment(w)).count();
        let (assignments, cmd_words) = words.split_at(assign_count);
        for a in assignments {
            let (k, v) = a.split_once('=').unwrap();
            self.env.export(k, v);
        }
        if cmd_words.is_empty() {
            return 0;
        }

        // Alias expansion — same logic as run_simple, needed for pipeline stages.
        let raw = &cmd_words[0];
        let (name, expanded_args): (String, Vec<String>) = if let Some(alias_val) =
            self.env.get_alias(raw)
        {
            let mut parts: Vec<String> = alias_val.split_whitespace().map(String::from).collect();
            let alias_name = if parts.is_empty() {
                raw.clone()
            } else {
                parts.remove(0)
            };
            parts.extend_from_slice(&cmd_words[1..]);
            (alias_name, parts)
        } else {
            (raw.clone(), cmd_words[1..].to_vec())
        };

        let args: Vec<&str> = expanded_args.iter().map(|s| s.as_str()).collect();

        if let Some(f) = lookup_builtin(name.as_str()) {
            return match f(self, &args) {
                Ok(s) => s.0,
                Err(e) => {
                    eprintln!("swagsh: {e}");
                    1
                }
            };
        }

        // Build full word list with alias-expanded name.
        let mut full_words = vec![name];
        full_words.extend_from_slice(&expanded_args);
        match self.do_exec(&full_words) {
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
    // Simple command (parent process)
    // ------------------------------------------------------------------

    fn run_simple(&mut self, sc: &SimpleCmd) -> Result<ExitStatus> {
        let words = self.expand_words(&sc.words)?;

        let assign_count = words.iter().take_while(|w| is_assignment(w)).count();
        let (assignments, cmd_words) = words.split_at(assign_count);

        // Pure assignment — no command.
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

        // Alias expansion — active in all execution modes within this session.
        // Non-recursive: the expanded command name is never re-expanded.
        let raw = &cmd_words[0];
        let (name, mut args): (String, Vec<String>) = if let Some(alias_val) =
            self.env.get_alias(raw)
        {
            let mut parts: Vec<String> = alias_val.split_whitespace().map(String::from).collect();
            let alias_name = if parts.is_empty() {
                raw.clone()
            } else {
                parts.remove(0)
            };
            parts.extend_from_slice(&cmd_words[1..]);
            (alias_name, parts)
        } else {
            (raw.clone(), cmd_words[1..].to_vec())
        };
        let _ = &mut args;
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let special = is_special_builtin(&name);

        // Builtin.
        if let Some(f) = lookup_builtin(name.as_str()) {
            // alias/unalias: expand each word fully (stripping quotes) but
            // do NOT IFS-split, so `ll='ls -la'` arrives as one token "ll=ls -la".
            let no_split_args: Vec<String> = if matches!(name.as_str(), "alias" | "unalias") {
                sc.words[assign_count + 1..]
                    .iter()
                    .map(|w| self.expand_word(w).map(|mut v| v.remove(0)))
                    .collect::<Result<Vec<_>>>()?
            } else {
                Vec::new()
            };

            let effective_arg_refs: Vec<&str> = if matches!(name.as_str(), "alias" | "unalias") {
                no_split_args.iter().map(|s| s.as_str()).collect()
            } else {
                arg_refs.clone()
            };

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
            let result = f(self, &effective_arg_refs);
            self.restore_fds(saved_fds)?;

            for (k, old) in saved_vars {
                match old {
                    Some(v) => self.env.set(&k, v),
                    None => self.env.unset(&k),
                }
            }
            return result;
        }

        // Shell function.
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

        // External command.
        // Rebuild full word list with the (possibly alias-expanded) name.
        // Use alias-expanded args (not cmd_words[1..]) so alias flags are included.
        let mut full_words = vec![name];
        full_words.extend_from_slice(&args);
        self.run_external_with_assignments(sc, &full_words, assignments)
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
                if self.interactive {
                    let my_pid = getpid();
                    let _ = setpgid(my_pid, my_pid);
                }
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
                if self.interactive {
                    let _ = setpgid(child, child);
                    let _ = tcsetpgrp_stdin(child);
                }
                let status = self.wait_for_pid(child)?;
                if self.interactive {
                    let _ = tcsetpgrp_stdin(self.shell_pgid);
                }
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
            if wc.until == cond.is_success() {
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
                    let fd = open_write(&self.word_to_path(&r.target)?, false)?;
                    dup2_raw(fd, r.fd)?;
                    close_raw(fd)?;
                }
                RedirectKind::Append => {
                    let fd = open_write(&self.word_to_path(&r.target)?, true)?;
                    dup2_raw(fd, r.fd)?;
                    close_raw(fd)?;
                }
                RedirectKind::In => {
                    let fd = open_read(&self.word_to_path(&r.target)?)?;
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
                    let fd = open_write(&self.word_to_path(&r.target)?, false)?;
                    dup2_raw(fd, 1)?;
                    dup2_raw(fd, 2)?;
                    close_raw(fd)?;
                }
                RedirectKind::HereString => {
                    // Always expand — the body may contain $VAR, $(cmd), etc.
                    let raw = match &r.target {
                        Word::Literal(s) => s.clone(),
                        other => self.expand_word_to_string(other)?,
                    };
                    // Expand each line individually preserving newlines.
                    let content = expand_heredoc_body(&raw, self)?;
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
    // Fd save/restore
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

    pub(crate) fn expand_word_to_string(&self, word: &Word) -> Result<String> {
        match word {
            Word::Literal(s) => {
                // Tilde expansion on bare literals starting with `~`.
                if s.starts_with('~') {
                    Ok(expand_tilde(s, &self.env))
                } else {
                    Ok(s.clone())
                }
            }
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

    /// Expand a `Command` node as a command substitution — used by the
    /// heredoc body expander which already has a parsed `Command`.
    pub(crate) fn expand_cmd_sub_cmd(&mut self, cmd: &Command) -> Result<String> {
        self.expand_cmd_sub(cmd)
    }

    fn expand_cmd_sub(&self, cmd: &Command) -> Result<String> {
        let (read_fd, write_fd) = raw_pipe()?;
        match unsafe { fork()? } {
            ForkResult::Child => {
                close_raw(read_fd)?;
                dup2_raw(write_fd, 1)?;
                close_raw(write_fd)?;
                let mut child_exec =
                    Executor::new(self.env.clone(), false).expect("executor init in cmd sub");
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
                Ok(String::from_utf8_lossy(&output)
                    .trim_end_matches('\n')
                    .to_owned())
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

fn builtin_alias(exec: &mut Executor, args: &[&str]) -> Result<ExitStatus> {
    if args.is_empty() {
        let mut pairs: Vec<(&str, &str)> = exec.env.all_aliases().collect();
        pairs.sort_by_key(|(k, _)| *k);
        for (k, v) in pairs {
            println!("alias {k}='{v}'");
        }
        return Ok(ExitStatus::SUCCESS);
    }
    for arg in args {
        // An arg like `ll=ls -la` arrives here already with quotes stripped
        // and the value intact as one string (the lexer consumed the quotes).
        // Split only on the FIRST `=` — everything after is the value.
        if let Some((k, v)) = arg.split_once('=') {
            exec.env.set_alias(k.to_owned(), v.to_owned());
        } else {
            match exec.env.get_alias(arg) {
                Some(v) => println!("alias {arg}='{v}'"),
                None => {
                    eprintln!("swagsh: alias: {arg}: not found");
                    return Ok(ExitStatus::FAILURE);
                }
            }
        }
    }
    Ok(ExitStatus::SUCCESS)
}

fn builtin_unalias(exec: &mut Executor, args: &[&str]) -> Result<ExitStatus> {
    if args.first() == Some(&"-a") {
        exec.env.clear_aliases();
        return Ok(ExitStatus::SUCCESS);
    }
    for arg in args {
        exec.env.remove_alias(arg);
    }
    Ok(ExitStatus::SUCCESS)
}

fn builtin_cd(exec: &mut Executor, args: &[&str]) -> Result<ExitStatus> {
    let target = match args.first() {
        Some(&"-") => exec.env.get("OLDPWD").unwrap_or_else(|| "/".into()),
        Some(&path) => expand_tilde(path, &exec.env),
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
    println!(
        "{}",
        std::env::current_dir()
            .map_err(|e| anyhow!("pwd: {e}"))?
            .display()
    );
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
    std::process::exit(args.first().and_then(|s| s.parse().ok()).unwrap_or(0));
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
            Pid::from_raw(
                target
                    .parse()
                    .map_err(|_| anyhow!("kill: invalid pid: {target}"))?,
            )
        };
        nix::sys::signal::kill(pid, sig)?;
    }
    Ok(ExitStatus::SUCCESS)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Tilde expansion — `~` → $HOME, `~user` not supported (uncommon, skip).
pub fn expand_tilde(s: &str, env: &Env) -> String {
    if s == "~" {
        return env.get("HOME").unwrap_or_else(|| "/".into());
    }
    if let Some(rest) = s.strip_prefix("~/") {
        let home = env.get("HOME").unwrap_or_else(|| "/".into());
        return format!("{home}/{rest}");
    }
    s.to_owned()
}

/// Glob expansion — handles path prefixes like `src/*.rs`.
fn glob_expand(pattern: &str) -> Vec<String> {
    // Split into directory prefix and filename pattern.
    let (dir, file_pat) = match pattern.rfind('/') {
        Some(pos) => (&pattern[..pos], &pattern[pos + 1..]),
        None => (".", pattern),
    };

    let Ok(entries) = std::fs::read_dir(dir) else {
        return vec![];
    };
    let mut results: Vec<String> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|name| glob_match(file_pat, name))
        .map(|name| {
            if dir == "." {
                name
            } else {
                format!("{dir}/{name}")
            }
        })
        .collect();
    results.sort();
    results
}

/// Expand variables and command substitutions inside a heredoc body.
/// Each `$VAR` / `$(cmd)` in the raw string is expanded; the overall
/// newline structure is preserved.
fn expand_heredoc_body(body: &str, exec: &Executor) -> Result<String> {
    // Parse the body as a double-quoted-like word so the existing
    // expand_word machinery handles $VAR and $() correctly.
    use crate::parser::parse;
    // Wrap in a no-op echo to get expansion, then discard the command.
    // Simpler: just do a lightweight variable substitution inline.
    let mut result = String::with_capacity(body.len());
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '$' {
            let mut var = String::new();
            match chars.peek().copied() {
                Some('{') => {
                    chars.next();
                    for ch in chars.by_ref() {
                        if ch == '}' {
                            break;
                        }
                        var.push(ch);
                    }
                    result.push_str(&exec.expand_var(&var));
                }
                Some('(') => {
                    // Command substitution — collect balanced parens.
                    chars.next();
                    let mut depth = 1usize;
                    let mut cmd_src = String::new();
                    for ch in chars.by_ref() {
                        if ch == '(' {
                            depth += 1;
                        } else if ch == ')' {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        cmd_src.push(ch);
                    }
                    if let Ok(program) = parse(&cmd_src) {
                        // Run in a sub-executor with the same env.
                        let mut sub = Executor::new(exec.env.clone(), false)
                            .unwrap_or_else(|_| panic!("sub executor"));
                        // Capture stdout via expand_cmd_sub path — reuse the AST.
                        if let Some(aol) = program.body.into_iter().next()
                            && let Some(item) = aol.items.into_iter().next()
                            && let Some(cmd) = item.command.commands.into_iter().next()
                            && let Ok(s) = sub.expand_cmd_sub_cmd(&cmd)
                        {
                            result.push_str(&s);
                        }
                    }
                }
                Some(c2) if c2.is_ascii_alphanumeric() || c2 == '_' || "@*#?-$!".contains(c2) => {
                    if "@*#?-$!".contains(c2) {
                        chars.next();
                        result.push_str(&exec.expand_var(&c2.to_string()));
                    } else {
                        // Collect the full identifier using while-let to avoid
                        // mixing for-loop borrows with peek().
                        while matches!(chars.peek(), Some(ch) if ch.is_ascii_alphanumeric() || *ch == '_')
                        {
                            var.push(chars.next().unwrap());
                        }
                        result.push_str(&exec.expand_var(&var));
                    }
                }
                _ => result.push('$'),
            }
        } else {
            result.push(c);
        }
    }
    // Ensure trailing newline.
    if !result.ends_with('\n') {
        result.push('\n');
    }
    Ok(result)
}

fn builtin_bracket(exec: &mut Executor, args: &[&str]) -> Result<ExitStatus> {
    // [ expr ] — last arg must be ]
    if args.last() != Some(&"]") {
        bail!("[: missing closing ]");
    }
    builtin_test(exec, &args[..args.len() - 1])
}

fn builtin_double_bracket(_exec: &mut Executor, args: &[&str]) -> Result<ExitStatus> {
    // args includes the trailing "]]" — strip it.
    if args.last() != Some(&"]]") {
        bail!("[[: missing closing ]]");
    }
    let inner = &args[..args.len() - 1];
    let result = eval_double_bracket(inner).unwrap_or(false);
    Ok(if result {
        ExitStatus::SUCCESS
    } else {
        ExitStatus::FAILURE
    })
}

/// Evaluate `[[ ]]` expressions — like eval_test but handles `&&` and `||`
/// as infix operators (they arrive as string tokens from the parser).
fn eval_double_bracket(args: &[&str]) -> Option<bool> {
    // Split on top-level || first (lowest precedence).
    let mut depth = 0usize;
    for (i, &tok) in args.iter().enumerate() {
        match tok {
            "(" => depth += 1,
            ")" => depth = depth.saturating_sub(1),
            "||" if depth == 0 => {
                let lhs = eval_double_bracket(&args[..i]).unwrap_or(false);
                if lhs {
                    return Some(true);
                }
                return eval_double_bracket(&args[i + 1..]);
            }
            _ => {}
        }
    }
    // Split on top-level &&.
    depth = 0;
    for (i, &tok) in args.iter().enumerate() {
        match tok {
            "(" => depth += 1,
            ")" => depth = depth.saturating_sub(1),
            "&&" if depth == 0 => {
                let lhs = eval_double_bracket(&args[..i]).unwrap_or(false);
                if !lhs {
                    return Some(false);
                }
                return eval_double_bracket(&args[i + 1..]);
            }
            _ => {}
        }
    }
    // No &&/|| at top level — fall through to standard test evaluator.
    eval_test(args)
}

fn builtin_test(_exec: &mut Executor, args: &[&str]) -> Result<ExitStatus> {
    let result = eval_test(args).unwrap_or(false);
    Ok(if result {
        ExitStatus::SUCCESS
    } else {
        ExitStatus::FAILURE
    })
}

fn builtin_read(exec: &mut Executor, args: &[&str]) -> Result<ExitStatus> {
    let var = args.first().copied().unwrap_or("REPLY");
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(0) => return Ok(ExitStatus::FAILURE), // EOF
        Ok(_) => {}
        Err(e) => bail!("read: {e}"),
    }
    let line = line.trim_end_matches('\n').trim_end_matches('\r');
    exec.env.set(var, line);
    Ok(ExitStatus::SUCCESS)
}

fn builtin_shift(exec: &mut Executor, args: &[&str]) -> Result<ExitStatus> {
    let n: usize = args.first().and_then(|s| s.parse().ok()).unwrap_or(1);
    let mut pos = exec.env.positional_args().to_vec();
    if n > pos.len() {
        bail!("shift: shift count out of range");
    }
    pos.drain(..n);
    exec.env.set_positional_args(pos);
    Ok(ExitStatus::SUCCESS)
}

// ---------------------------------------------------------------------------
// test / [ / [[ expression evaluator
// ---------------------------------------------------------------------------

/// Evaluate a test expression given as a slice of string tokens.
/// Returns `true` for success (exit 0), `false` for failure (exit 1).
fn eval_test(args: &[&str]) -> Option<bool> {
    let (result, rest): (bool, &[&str]) = parse_or(args)?;
    if !rest.is_empty() {
        return None;
    }
    Some(result)
}

// or-expression: expr (-o expr)*
fn parse_or<'a>(args: &'a [&'a str]) -> Option<(bool, &'a [&'a str])> {
    let (mut val, mut rest) = parse_and(args)?;
    while rest.first() == Some(&"-o") {
        let (rhs, r2) = parse_and(&rest[1..])?;
        val = val || rhs;
        rest = r2;
    }
    Some((val, rest))
}

// and-expression: expr (-a expr)*
fn parse_and<'a>(args: &'a [&'a str]) -> Option<(bool, &'a [&'a str])> {
    let (mut val, mut rest) = parse_not(args)?;
    while rest.first() == Some(&"-a") {
        let (rhs, r2) = parse_not(&rest[1..])?;
        val = val && rhs;
        rest = r2;
    }
    Some((val, rest))
}

// not-expression: ! expr | primary
fn parse_not<'a>(args: &'a [&'a str]) -> Option<(bool, &'a [&'a str])> {
    if args.first() == Some(&"!") {
        let (val, rest) = parse_not(&args[1..])?;
        return Some((!val, rest));
    }
    // parenthesised group for [[ ]]
    if args.first() == Some(&"(") {
        let close = args.iter().rposition(|&a| a == ")")?;
        let inner = &args[1..close];
        let (val, _) = parse_or(inner)?;
        return Some((val, &args[close + 1..]));
    }
    parse_primary(args)
}

// primary: unary-op arg | arg binary-op arg | arg
fn parse_primary<'a>(args: &'a [&'a str]) -> Option<(bool, &'a [&'a str])> {
    match args {
        [] => Some((false, &[])),

        // ── Unary file tests ──────────────────────────────────────────────
        [op, path, rest @ ..]
            if matches!(
                *op,
                "-e" | "-f"
                    | "-d"
                    | "-r"
                    | "-w"
                    | "-x"
                    | "-s"
                    | "-L"
                    | "-h"
                    | "-b"
                    | "-c"
                    | "-p"
                    | "-S"
                    | "-u"
                    | "-g"
                    | "-k"
            ) =>
        {
            use std::fs;
            use std::os::unix::fs::MetadataExt;
            let p = std::path::Path::new(path);
            let val = match *op {
                "-e" => p.exists(),
                "-f" => p.is_file(),
                "-d" => p.is_dir(),
                "-L" | "-h" => p
                    .symlink_metadata()
                    .map(|m| m.file_type().is_symlink())
                    .unwrap_or(false),
                "-r" => fs::metadata(p)
                    .map(|m| {
                        !m.permissions().readonly()
                            || unsafe {
                                libc::access(
                                    std::ffi::CString::new(*path).unwrap().as_ptr(),
                                    libc::R_OK,
                                ) == 0
                            }
                    })
                    .unwrap_or(false),
                "-w" => unsafe {
                    !p.exists()
                        || libc::access(std::ffi::CString::new(*path).unwrap().as_ptr(), libc::W_OK)
                            == 0
                },
                "-x" => unsafe {
                    libc::access(std::ffi::CString::new(*path).unwrap().as_ptr(), libc::X_OK) == 0
                },
                "-s" => fs::metadata(p).map(|m| m.size() > 0).unwrap_or(false),
                "-b" => fs::metadata(p)
                    .map(|m| m.file_type().is_block_device())
                    .unwrap_or(false),
                "-c" => fs::metadata(p)
                    .map(|m| m.file_type().is_char_device())
                    .unwrap_or(false),
                "-p" => fs::metadata(p)
                    .map(|m| m.file_type().is_fifo())
                    .unwrap_or(false),
                "-S" => fs::metadata(p)
                    .map(|m| m.file_type().is_socket())
                    .unwrap_or(false),
                "-u" => fs::metadata(p)
                    .map(|m| m.mode() & 0o4000 != 0)
                    .unwrap_or(false),
                "-g" => fs::metadata(p)
                    .map(|m| m.mode() & 0o2000 != 0)
                    .unwrap_or(false),
                "-k" => fs::metadata(p)
                    .map(|m| m.mode() & 0o1000 != 0)
                    .unwrap_or(false),
                _ => false,
            };
            Some((val, rest))
        }

        // ── Unary string tests ────────────────────────────────────────────
        ["-z", s, rest @ ..] => Some((s.is_empty(), rest)),
        ["-n", s, rest @ ..] => Some((!s.is_empty(), rest)),
        ["-t", fd_str, rest @ ..] => {
            let fd: i32 = fd_str.parse().unwrap_or(-1);
            let val = unsafe { libc::isatty(fd) == 1 };
            Some((val, rest))
        }

        // ── Binary string tests ───────────────────────────────────────────
        [a, "=", b, rest @ ..] => Some((a == b, rest)),
        [a, "==", b, rest @ ..] => Some((a == b, rest)),
        [a, "!=", b, rest @ ..] => Some((a != b, rest)),
        [a, "<", b, rest @ ..] => Some((a < b, rest)),
        [a, ">", b, rest @ ..] => Some((a > b, rest)),

        // ── Binary integer tests ──────────────────────────────────────────
        [a, op, b, rest @ ..] if matches!(*op, "-eq" | "-ne" | "-lt" | "-le" | "-gt" | "-ge") => {
            let ai: i64 = a.parse().unwrap_or(0);
            let bi: i64 = b.parse().unwrap_or(0);
            let val = match *op {
                "-eq" => ai == bi,
                "-ne" => ai != bi,
                "-lt" => ai < bi,
                "-le" => ai <= bi,
                "-gt" => ai > bi,
                "-ge" => ai >= bi,
                _ => false,
            };
            Some((val, rest))
        }

        // ── Bare string (non-empty = true) ────────────────────────────────
        [s, rest @ ..] => Some((!s.is_empty(), rest)),
    }
}

fn glob_match(pattern: &str, text: &str) -> bool {
    glob_match_inner(
        &pattern.chars().collect::<Vec<_>>(),
        &text.chars().collect::<Vec<_>>(),
    )
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
