use std::ffi::CString;
use std::path::PathBuf;

use anyhow::{Result, anyhow, bail};
use rustix::fd::RawFd;
use rustix::io::Errno;
use rustix::process::{Pid, WaitOptions, getpid, setpgid, waitpid};
use rustix::runtime::{Fork, execve, kernel_fork};
use rustix::termios::tcsetpgrp;

use crate::ast::{Command, Pipeline, Redirect, RedirectKind, SimpleCmd, Word};
use crate::builtins;
use crate::fd::{close_raw, dup2_raw, open_read, open_write, raw_pipe, write_raw};
use crate::jobs::{ExitStatus, JobState};
use crate::signal::restore_child_signals;

use super::{Shell, is_assignment, is_break, is_continue, is_return, resolve_alias};

impl Shell {
    pub fn run_pipeline(&mut self, pipeline: &Pipeline) -> Result<ExitStatus> {
        let n = pipeline.commands.len();

        if n == 1 {
            let mut status = self.run_command(&pipeline.commands[0])?;
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

        let pgid = pgid.unwrap();
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
            if last_status.0 != 130
                && let Some(id) = job_id
            {
                self.jobs.remove(id);
            }
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

    pub fn run_pipeline_async(&mut self, pipeline: &Pipeline) -> Result<ExitStatus> {
        let n = pipeline.commands.len();
        let mut pgid: Option<Pid> = None;
        let mut pids = Vec::with_capacity(n);
        let mut prev_read: Option<RawFd> = None;
        let cmd_str = format!("{n} commands");

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

        let pgid = pgid.unwrap();
        let job_id = self.jobs.add(pgid, pids, cmd_str);
        eprintln!("[{job_id}] {pgid}");
        Ok(ExitStatus::SUCCESS)
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
                        eprintln!("swagsh: {e}");
                    }
                    1
                }
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

        let (name, expanded_args) = resolve_alias(&self.env, &cmd_words[0], &cmd_words[1..]);
        let arg_refs: Vec<&str> = expanded_args.iter().map(String::as_str).collect();

        if let Some(f) = builtins::lookup_builtin(name.as_str()) {
            return match f(self, &arg_refs) {
                Ok(s) => s.0,
                Err(e) => {
                    eprintln!("swagsh: {e}");
                    1
                }
            };
        }

        let mut full_words = vec![name];
        full_words.extend(expanded_args);
        match Self::do_exec(&full_words) {
            Ok(_) => unreachable!(),
            Err(e) => {
                eprintln!("swagsh: {e}");
                127
            }
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
                    let (k, v) = a.split_once('=').unwrap();
                    self.env.export(k, v);
                }
                if let Err(e) = self.apply_redirects(&sc.redirects) {
                    eprintln!("swagsh: {e}");
                    std::process::exit(1);
                }
                match Self::do_exec(words) {
                    Ok(_) => unreachable!(),
                    Err(e) => {
                        eprintln!("swagsh: {e}");
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
            .map(|w| CString::new(w.as_str()).map_err(|e| anyhow!(e)))
            .collect::<Result<_>>()?;
        let errno = execvp_path(&argv);
        bail!("{}: {}", words[0], errno);
    }

    pub fn wait_for_pid(&mut self, pid: Pid) -> Result<ExitStatus> {
        loop {
            match waitpid(Some(pid), WaitOptions::UNTRACED) {
                Ok(Some((_, status))) => {
                    if let Some(code) = status.exit_status() {
                        return Ok(ExitStatus(code));
                    } else if let Some(sig) = status.terminating_signal() {
                        return Ok(ExitStatus(128 + sig));
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
                Err(e) => return Err(anyhow!(e)),
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
