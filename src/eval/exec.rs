use std::ffi::CString;
use std::path::PathBuf;

use anyhow::{Result, anyhow, bail};
use rustix::fd::RawFd;
use rustix::io::Errno;
use rustix::process::{Pid, WaitOptions, getpid, setpgid, waitpid};
use rustix::runtime::{Fork, execve, kernel_fork};
use rustix::termios::tcsetpgrp;

use crate::ast::{Command, Pipeline, Redirect, RedirectKind, SimpleCmd, Word};
use crate::errfmt::{emit, strerror};
use crate::fd::{close_raw, dup2_raw, open_read, open_write, raw_pipe, write_raw};
use crate::jobs::{ExitStatus, JobState, decode_wait_status};
use crate::signal::restore_child_signals;

use super::{Resolved, Shell, catch_return, is_break, is_continue, is_return};

impl Shell {
    pub fn run_pipeline(&mut self, pipeline: &Pipeline) -> Result<ExitStatus> {
        let n = pipeline.commands.len();

        if n == 1 {
            let mut status = self.run_command(&pipeline.commands[0])?;
            if pipeline.negated {
                status = status.negated();
            }
            return Ok(status);
        }

        let (pgid, pids) = self.spawn_pipeline(pipeline)?;
        if self.interactive {
            let _ = tcsetpgrp(std::io::stdin(), pgid);
        }

        let job_id = if self.interactive {
            Some(
                self.jobs
                    .add(pgid, pids.clone(), format!("pipeline ({n} stages)")),
            )
        } else {
            None
        };

        let mut last_status = ExitStatus::SUCCESS;
        for child in &pids {
            last_status = self.wait_for_pid(*child)?;
        }

        if self.interactive {
            let _ = tcsetpgrp(std::io::stdin(), self.pgid);
            self.restore_terminal();
            if last_status.0 != 130
                && let Some(id) = job_id
            {
                self.jobs.remove(id);
            }
        }

        if pipeline.negated {
            last_status = last_status.negated();
        }
        Ok(last_status)
    }

    pub fn run_pipeline_async(&mut self, pipeline: &Pipeline) -> Result<ExitStatus> {
        let label = describe_pipeline(pipeline);
        let (pgid, pids) = self.spawn_pipeline(pipeline)?;
        let job_id = self.jobs.add(pgid, pids, label);
        eprintln!("[{job_id}] {pgid}");
        Ok(ExitStatus::SUCCESS)
    }

    /// Forks every stage of `pipeline`, wiring each stage's stdout to the
    /// next stage's stdin via a pipe, and placing them all in one process
    /// group. Returns the group's pgid and the pids of every forked stage,
    /// in pipeline order. Shared by `run_pipeline` (waits synchronously) and
    /// `run_pipeline_async` (registers a background job and returns).
    fn spawn_pipeline(&mut self, pipeline: &Pipeline) -> Result<(Pid, Vec<Pid>)> {
        let n = pipeline.commands.len();
        let mut pgid: Option<Pid> = None;
        let mut pids = Vec::with_capacity(n);
        let mut prev_read: Option<RawFd> = None;

        for (idx, cmd) in pipeline.commands.iter().enumerate() {
            let is_last = idx == n - 1;
            let (pipe_read, pipe_write) = if is_last {
                (None, None)
            } else {
                let (r, w) = raw_pipe()?;
                (Some(r), Some(w))
            };

            let child = self.fork_command(cmd, prev_read, pipe_write, pgid)?;

            if let Some(existing) = pgid {
                let _ = setpgid(Some(child), Some(existing));
            } else {
                pgid = Some(child);
                let _ = setpgid(Some(child), Some(child));
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

        Ok((pgid.unwrap(), pids))
    }

    fn fork_command(
        &mut self,
        cmd: &Command,
        stdin_override: Option<RawFd>,
        stdout_override: Option<RawFd>,
        pgid: Option<Pid>,
    ) -> Result<Pid> {
        // SAFETY: fork rules: async-signal-safe code only in child until exec.
        match unsafe { kernel_fork()? } {
            Fork::Child(_) => {
                // SAFETY: in child, before any allocations.
                unsafe { restore_child_signals() };
                let my_pid = getpid();
                let group = pgid.unwrap_or(my_pid);
                let _ = setpgid(Some(my_pid), Some(group));
                if let Some(fd) = stdin_override {
                    let _ = dup2_raw(fd, 0);
                    close_raw(fd);
                }
                if let Some(fd) = stdout_override {
                    let _ = dup2_raw(fd, 1);
                    close_raw(fd);
                }
                let status = self.exec_in_child(cmd);
                std::process::exit(status);
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
            Resolved::Function(body, args) => {
                self.env.set_positional_args(args);
                match catch_return(self.run_command(&body)) {
                    Ok(s) => s.0,
                    Err(e) => {
                        emit(e);
                        1
                    }
                }
            }
            Resolved::External(words) => match Self::do_exec(&words) {
                Ok(_) => unreachable!(),
                Err(e) => {
                    emit(e);
                    127
                }
            },
        }
    }

    pub(super) fn run_external(
        &mut self,
        sc: &SimpleCmd,
        words: &[String],
        assignments: &[String],
    ) -> Result<ExitStatus> {
        // SAFETY: fork; async-signal-safe code only in child.
        match unsafe { kernel_fork()? } {
            Fork::Child(_) => {
                // SAFETY: in child, before any allocations.
                unsafe { restore_child_signals() };
                if self.interactive {
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
                match Self::do_exec(words) {
                    Ok(_) => unreachable!(),
                    Err(e) => {
                        emit(e);
                        std::process::exit(127);
                    }
                }
            }
            Fork::ParentOf(child) => {
                if self.interactive {
                    let _ = setpgid(Some(child), Some(child));
                    let _ = tcsetpgrp(std::io::stdin(), child);
                }
                let job_id = if self.interactive {
                    Some(self.jobs.add(child, vec![child], words.join(" ")))
                } else {
                    None
                };
                let status = self.wait_for_pid(child)?;
                if self.interactive {
                    let _ = tcsetpgrp(std::io::stdin(), self.pgid);
                    self.restore_terminal();
                    if status.0 != 130
                        && let Some(id) = job_id
                    {
                        self.jobs.remove(id);
                    }
                }
                Ok(status)
            }
        }
    }

    pub fn do_exec(words: &[String]) -> Result<std::convert::Infallible> {
        if words.is_empty() {
            bail!("exec: no command");
        }
        let argv: Vec<CString> = words
            .iter()
            .map(|w| CString::new(w.as_str()))
            .collect::<Result<_, _>>()?;
        let errno = execvp_path(&argv);
        bail!("{}: {}", words[0], strerror(errno));
    }

    pub fn wait_for_pid(&mut self, pid: Pid) -> Result<ExitStatus> {
        loop {
            match waitpid(Some(pid), WaitOptions::UNTRACED) {
                Ok(Some((_, status))) => {
                    if let Some(exit) = decode_wait_status(status) {
                        return Ok(exit);
                    } else if status.stopped() {
                        if let Some(job) = self.jobs.by_pgid_mut(pid) {
                            job.state = JobState::Stopped;
                            eprintln!("\n[{}]+ Stopped\t{}", job.id, job.command);
                        }
                        return Ok(ExitStatus(130));
                    }
                }
                Ok(None) => {}
                Err(e) if e == Errno::INTR => {}
                Err(e) => return Err(e.into()),
            }
        }
    }

    pub(super) fn apply_redirects(&mut self, redirects: &[Redirect]) -> Result<()> {
        for r in redirects {
            match &r.kind {
                RedirectKind::Out => {
                    let fd = open_write(&self.word_to_path(&r.target)?, false)?;
                    dup2_raw(fd, r.fd)?;
                    close_raw(fd);
                }
                RedirectKind::Append => {
                    let fd = open_write(&self.word_to_path(&r.target)?, true)?;
                    dup2_raw(fd, r.fd)?;
                    close_raw(fd);
                }
                RedirectKind::In => {
                    let fd = open_read(&self.word_to_path(&r.target)?)?;
                    dup2_raw(fd, r.fd)?;
                    close_raw(fd);
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
                    close_raw(fd);
                }
                RedirectKind::HereDoc { raw_body, quoted } => {
                    let content = if *quoted {
                        let mut s = raw_body.clone();
                        if !s.ends_with('\n') {
                            s.push('\n');
                        }
                        s
                    } else {
                        self.expand_heredoc_body(raw_body)
                    };
                    write_herestring(&content)?;
                }
                RedirectKind::HereString => {
                    let raw = match &r.target {
                        Word::Literal(s) => s.clone(),
                        other => self.expand_word_to_string(other)?,
                    };
                    let content = self.expand_heredoc_body(&raw);
                    write_herestring(&content)?;
                }
            }
        }
        Ok(())
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
        if let Ok(Fork::Child(_)) = unsafe { kernel_fork() } {
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
        Command::For(_) => "for ...".to_owned(),
        Command::While(_) => "while ...".to_owned(),
        Command::Case(_) => "case ...".to_owned(),
        Command::Group(gc) if gc.subshell => "(...)".to_owned(),
        Command::Group(_) => "{ ... }".to_owned(),
        Command::FunctionDef(fd) => format!("{}()", fd.name),
    }
}

fn describe_word(word: &Word) -> String {
    match word {
        Word::Literal(s) => s.clone(),
        Word::Var(name) => format!("${name}"),
        Word::Arith(expr) => format!("$(({expr}))"),
        Word::CmdSub(_) => "$(...)".to_owned(),
        Word::Compound(parts) => parts.iter().map(describe_word).collect(),
        Word::Quoted(inner) => describe_word(inner),
    }
}

fn execvp_path(argv: &[CString]) -> rustix::io::Errno {
    let mut argv_ptrs: Vec<*const u8> = argv.iter().map(|s| s.as_ptr().cast::<u8>()).collect();
    argv_ptrs.push(std::ptr::null());

    let env_cstrings: Vec<CString> = std::env::vars_os()
        .filter_map(|(k, v)| {
            let mut kv = k.into_encoded_bytes();
            kv.push(b'=');
            kv.extend(v.into_encoded_bytes());
            CString::new(kv).ok()
        })
        .collect();
    let mut envp_ptrs: Vec<*const u8> = env_cstrings
        .iter()
        .map(|s| s.as_ptr().cast::<u8>())
        .collect();
    envp_ptrs.push(std::ptr::null());

    let name = &argv[0];
    let name_bytes = name.to_bytes();

    if name_bytes.contains(&b'/') {
        // SAFETY: null-terminated arrays of valid CString data.
        return unsafe { execve(name, argv_ptrs.as_ptr(), envp_ptrs.as_ptr()) };
    }

    let path_var =
        std::env::var_os("PATH").unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin".into());
    let mut last_err = rustix::io::Errno::NOENT;
    for dir in std::env::split_paths(&path_var) {
        let mut full = dir;
        full.push(std::str::from_utf8(name_bytes).unwrap_or(""));
        if let Ok(candidate) = CString::new(full.as_os_str().as_encoded_bytes()) {
            argv_ptrs[0] = candidate.as_ptr().cast::<u8>();
            // SAFETY: null-terminated arrays of valid CString data.
            let err = unsafe { execve(&candidate, argv_ptrs.as_ptr(), envp_ptrs.as_ptr()) };
            if err != rustix::io::Errno::NOENT && err != rustix::io::Errno::NOTDIR {
                return err;
            }
            last_err = err;
        }
    }
    last_err
}
