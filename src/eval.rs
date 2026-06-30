use std::ffi::CString;
use std::fmt;
use std::path::PathBuf;

use anyhow::{Error, Result, anyhow, bail};
use rustix::fd::RawFd;
use rustix::io::Errno;
use rustix::process::{Pid, Signal, WaitOptions, getpid, setpgid, waitpid};
use rustix::runtime::{Fork, execve, kernel_fork, kernel_sigaction};
use rustix::termios::tcsetpgrp;

use crate::ast::{
    AndOrList, AndOrOp, CaseClause, Command, ForClause, GroupCmd, IfClause, Pipeline, Program,
    Redirect, RedirectKind, SimpleCmd, WhileClause, Word,
};
use crate::builtins::{self, BuiltinFn};
use crate::env::Env;
use crate::expand::{
    ParamOp, eval_arith, expand_tilde, glob_expand, glob_match, parse_param_op, strip_prefix,
    strip_suffix,
};
use crate::fd::{
    close_raw, dup2_raw, open_read, open_write, raw_pipe, read_raw, restore_fds, save_fds,
    write_raw,
};
use crate::jobs::{ExitStatus, JobState, JobTable};
use crate::signal::{restore_child_signals, sig_ign_action};

// ---------------------------------------------------------------------------
// Control-flow signals: propagate through the call stack via anyhow::Error.
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum LoopSignal {
    Break,
    Continue,
}

impl fmt::Display for LoopSignal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Break => f.write_str("break outside loop"),
            Self::Continue => f.write_str("continue outside loop"),
        }
    }
}

impl std::error::Error for LoopSignal {}

#[derive(Debug)]
pub struct ReturnSignal(pub i32);

impl fmt::Display for ReturnSignal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "return {}", self.0)
    }
}

impl std::error::Error for ReturnSignal {}

pub fn is_break(e: &Error) -> bool {
    matches!(e.downcast_ref::<LoopSignal>(), Some(LoopSignal::Break))
}

pub fn is_continue(e: &Error) -> bool {
    matches!(e.downcast_ref::<LoopSignal>(), Some(LoopSignal::Continue))
}

pub fn is_return(e: &Error) -> bool {
    e.downcast_ref::<ReturnSignal>().is_some()
}

// ---------------------------------------------------------------------------
// Shell: the shared execution context.
// ---------------------------------------------------------------------------

pub struct Shell {
    pub env: Env,
    pub jobs: JobTable,
    pub pgid: Pid,
    pub last_status: ExitStatus,
    pub interactive: bool,
}

impl Shell {
    pub fn new(env: Env, interactive: bool) -> Self {
        let pgid = getpid();
        if interactive {
            let _ = setpgid(Some(pgid), Some(pgid));
            let _ = tcsetpgrp(std::io::stdin(), pgid);
            // SAFETY: main shell process, single-threaded at startup.
            unsafe {
                let ign = sig_ign_action();
                let _ = kernel_sigaction(Signal::TTOU, Some(ign.clone()));
                let _ = kernel_sigaction(Signal::TTIN, Some(ign.clone()));
                let _ = kernel_sigaction(Signal::TSTP, Some(ign));
            }
        }
        Self {
            env,
            jobs: JobTable::default(),
            pgid,
            last_status: ExitStatus::SUCCESS,
            interactive,
        }
    }

    // ------------------------------------------------------------------
    // Top level
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
        let items = &aol.items;
        let mut status = ExitStatus::SUCCESS;
        let mut i = 0;
        while i < items.len() {
            let is_last = i == items.len() - 1;
            status = if aol.is_async && is_last {
                self.run_pipeline_async(&items[i].command)?
            } else {
                self.run_pipeline(&items[i].command)?
            };
            match items[i].op {
                None => break,
                Some(AndOrOp::And) if !status.is_success() => {
                    i += 1;
                    while i < items.len() && items[i - 1].op != Some(AndOrOp::Or) {
                        i += 1;
                    }
                }
                Some(AndOrOp::Or) if status.is_success() => {
                    i += 1;
                    while i < items.len() && items[i - 1].op != Some(AndOrOp::And) {
                        i += 1;
                    }
                }
                _ => i += 1,
            }
        }
        self.last_status = status;
        Ok(status)
    }

    // ------------------------------------------------------------------
    // Pipeline
    // ------------------------------------------------------------------

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

    fn run_pipeline_async(&mut self, pipeline: &Pipeline) -> Result<ExitStatus> {
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

    // ------------------------------------------------------------------
    // Command dispatch
    // ------------------------------------------------------------------

    pub fn run_command(&mut self, cmd: &Command) -> Result<ExitStatus> {
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

        // Assignment-only statement: apply redirects, set vars, done.
        if cmd_words.is_empty() {
            let saved = save_fds(&[0, 1, 2])?;
            self.apply_redirects(&sc.redirects)?;
            restore_fds(saved)?;
            for assign in assignments {
                let (name, value) = assign.split_once('=').unwrap();
                self.env.set(name, value);
            }
            return Ok(ExitStatus::SUCCESS);
        }

        let (name, args) = resolve_alias(&self.env, &cmd_words[0], &cmd_words[1..]);
        let special = is_special_builtin(&name);

        if let Some(f) = builtins::lookup_builtin(name.as_str()) {
            return self.run_builtin(sc, f, &name, &args, assignments, special);
        }

        if let Some(body) = self.env.get_function(&name).cloned() {
            return self.run_function(sc, &body, &args, assignments);
        }

        let mut full_words = vec![name];
        full_words.extend(args);
        self.run_external(sc, &full_words, assignments)
    }

    fn run_builtin(
        &mut self,
        sc: &SimpleCmd,
        f: BuiltinFn,
        name: &str,
        args: &[String],
        assignments: &[String],
        special: bool,
    ) -> Result<ExitStatus> {
        // alias/unalias receive verbatim (non-IFS-split) expanded args.
        let verbatim: Option<Vec<String>> = if matches!(name, "alias" | "unalias") {
            let assign_count = sc.words.iter().take_while(|w| {
                matches!(self.expand_word(w), Ok(v) if v.first().is_some_and(|s| is_assignment(s)))
            }).count();
            Some(
                sc.words[assign_count + 1..]
                    .iter()
                    .map(|w| self.expand_word(w).map(|mut v| v.remove(0)))
                    .collect::<Result<Vec<_>>>()?,
            )
        } else {
            None
        };

        let arg_refs: Vec<&str> = verbatim.as_ref().map_or_else(
            || args.iter().map(String::as_str).collect(),
            |v| v.iter().map(String::as_str).collect(),
        );

        let saved_vars: Vec<(String, Option<String>)> = if special {
            for a in assignments {
                let (k, v) = a.split_once('=').unwrap();
                self.env.set(k, v);
            }
            vec![]
        } else {
            assignments
                .iter()
                .map(|a| {
                    let (k, v) = a.split_once('=').unwrap();
                    let old = self.env.get(k);
                    self.env.set(k, v);
                    (k.to_owned(), old)
                })
                .collect()
        };

        let saved_fds = save_fds(&[0, 1, 2])?;
        let result = if let Err(e) = self.apply_redirects(&sc.redirects) {
            Err(e)
        } else {
            f(self, &arg_refs)
        };
        let _ = restore_fds(saved_fds);
        self.env.restore_vars(saved_vars);
        result
    }

    fn run_function(
        &mut self,
        sc: &SimpleCmd,
        body: &Command,
        args: &[String],
        assignments: &[String],
    ) -> Result<ExitStatus> {
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
        self.env.set_positional_args(args.to_vec());
        let saved_fds = save_fds(&[0, 1, 2])?;
        let result = if let Err(e) = self.apply_redirects(&sc.redirects) {
            Err(e)
        } else {
            self.run_command(body)
        };
        let _ = restore_fds(saved_fds);
        self.env.set_positional_args(old_args);
        self.env.restore_vars(saved_vars);

        // Catch `return` at the function boundary.
        match result {
            Err(e) if is_return(&e) => Ok(ExitStatus(e.downcast::<ReturnSignal>().unwrap().0)),
            other => other,
        }
    }

    fn run_external(
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

    // ------------------------------------------------------------------
    // execvp: PATH-searching exec via rustix
    // ------------------------------------------------------------------

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
                Err(e) if is_continue(&e) => {}
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
                Err(e) if is_continue(&e) => {}
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
                let pat = self.expand_word_to_string(pattern)?;
                if glob_match(&pat, &word) {
                    return self.run_list(&arm.body);
                }
            }
        }
        Ok(ExitStatus::SUCCESS)
    }

    fn run_group(&mut self, gc: &GroupCmd) -> Result<ExitStatus> {
        if gc.subshell {
            // SAFETY: fork.
            match unsafe { kernel_fork()? } {
                Fork::Child(_) => {
                    let status = self.run_list(&gc.body).unwrap_or(ExitStatus::FAILURE);
                    std::process::exit(status.0);
                }
                Fork::ParentOf(child) => return self.wait_for_pid(child),
            }
        }
        self.run_list(&gc.body)
    }

    pub fn run_list(&mut self, list: &[AndOrList]) -> Result<ExitStatus> {
        let mut status = ExitStatus::SUCCESS;
        for aol in list {
            status = self.run_and_or(aol)?;
        }
        Ok(status)
    }

    // ------------------------------------------------------------------
    // Redirections
    // ------------------------------------------------------------------

    fn apply_redirects(&mut self, redirects: &[Redirect]) -> Result<()> {
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

    // ------------------------------------------------------------------
    // Wait for child
    // ------------------------------------------------------------------

    fn wait_for_pid(&mut self, pid: Pid) -> Result<ExitStatus> {
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

    // ------------------------------------------------------------------
    // Word expansion
    // ------------------------------------------------------------------

    pub fn expand_word(&mut self, word: &Word) -> Result<Vec<String>> {
        if let Word::Quoted(inner) = word {
            return Ok(vec![self.expand_word_to_string_inner(inner, true)?]);
        }
        let raw = self.expand_word_to_string(word)?;
        if raw.contains('*') || raw.contains('?') || raw.contains('[') {
            let matches = glob_expand(&raw);
            if !matches.is_empty() {
                return Ok(matches);
            }
        }
        Ok(vec![raw])
    }

    pub fn expand_word_to_string(&mut self, word: &Word) -> Result<String> {
        self.expand_word_to_string_inner(word, false)
    }

    fn expand_word_to_string_inner(&mut self, word: &Word, in_quotes: bool) -> Result<String> {
        match word {
            Word::Literal(s) => {
                if !in_quotes && s.starts_with('~') {
                    Ok(expand_tilde(s, &self.env))
                } else {
                    Ok(s.clone())
                }
            }
            Word::Var(name) => Ok(self.expand_var(name)),
            Word::Arith(expr) => {
                let expanded = self.expand_arith_vars(expr);
                Ok(eval_arith(&expanded).to_string())
            }
            Word::CmdSub(cmd) => self.expand_cmd_sub(cmd),
            Word::Compound(parts) => {
                let mut result = String::new();
                for part in parts {
                    result.push_str(&self.expand_word_to_string_inner(part, in_quotes)?);
                }
                Ok(result)
            }
            Word::Quoted(inner) => self.expand_word_to_string_inner(inner, true),
        }
    }

    fn expand_arith_vars(&self, expr: &str) -> String {
        let mut result = String::new();
        let mut chars = expr.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '$' {
                result.push(c);
                continue;
            }
            let mut var = String::new();
            match chars.peek() {
                Some(&'{') => {
                    chars.next();
                    for ch in chars.by_ref() {
                        if ch == '}' {
                            break;
                        }
                        var.push(ch);
                    }
                }
                Some(&'?') => {
                    chars.next();
                    result.push_str(&self.last_status.0.to_string());
                    continue;
                }
                _ => {
                    while matches!(chars.peek(), Some(c) if c.is_ascii_alphanumeric() || *c == '_')
                    {
                        var.push(chars.next().unwrap());
                    }
                }
            }
            result.push_str(&self.env.get(&var).unwrap_or_default());
        }
        result
    }

    pub fn expand_var(&mut self, name: &str) -> String {
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
            name => match parse_param_op(name) {
                Some(ParamOp::Length(var)) => {
                    self.env.get(var).unwrap_or_default().len().to_string()
                }
                Some(ParamOp::PrefixStrip { var, pat, greedy }) => {
                    let val = self.env.get(var).unwrap_or_default();
                    strip_prefix(&val, pat, greedy)
                }
                Some(ParamOp::SuffixStrip { var, pat, greedy }) => {
                    let val = self.env.get(var).unwrap_or_default();
                    strip_suffix(&val, pat, greedy)
                }
                Some(ParamOp::Conditional { var, op, word }) => {
                    let val = self.env.get(var).unwrap_or_default();
                    match op {
                        ":-" => {
                            if val.is_empty() {
                                word.to_owned()
                            } else {
                                val
                            }
                        }
                        ":+" => {
                            if val.is_empty() {
                                String::new()
                            } else {
                                word.to_owned()
                            }
                        }
                        ":?" => {
                            if val.is_empty() {
                                let msg = if word.is_empty() {
                                    "parameter not set"
                                } else {
                                    word
                                };
                                eprintln!("swagsh: {var}: {msg}");
                                if !self.interactive {
                                    std::process::exit(1);
                                }
                            }
                            val
                        }
                        ":=" => {
                            if val.is_empty() {
                                self.env.set(var, word);
                                word.to_owned()
                            } else {
                                val
                            }
                        }
                        _ => val,
                    }
                }
                None => self.env.get(name).unwrap_or_default(),
            },
        }
    }

    fn expand_cmd_sub(&self, cmd: &Command) -> Result<String> {
        let (read_fd, write_fd) = raw_pipe()?;
        // SAFETY: fork.
        match unsafe { kernel_fork()? } {
            Fork::Child(_) => {
                close_raw(read_fd);
                let _ = dup2_raw(write_fd, 1);
                close_raw(write_fd);
                let mut child = Self::new(self.env.clone(), false);
                let status = child.run_command(cmd).unwrap_or(ExitStatus::FAILURE);
                std::process::exit(status.0);
            }
            Fork::ParentOf(child) => {
                close_raw(write_fd);
                let mut output = Vec::new();
                let mut buf = [0u8; 512];
                loop {
                    match read_raw(read_fd, &mut buf) {
                        Ok(0) => break,
                        Ok(n) => output.extend_from_slice(&buf[..n]),
                        Err(e) if e == Errno::INTR => {}
                        Err(e) => return Err(anyhow!(e)),
                    }
                }
                close_raw(read_fd);
                let _ = waitpid(Some(child), WaitOptions::empty());
                Ok(String::from_utf8_lossy(&output)
                    .trim_end_matches('\n')
                    .to_owned())
            }
        }
    }

    pub fn expand_words(&mut self, words: &[Word]) -> Result<Vec<String>> {
        let ifs = self.env.get("IFS").unwrap_or_else(|| " \t\n".to_owned());
        let mut result = Vec::with_capacity(words.len());
        for word in words {
            let split = matches!(word, Word::Var(_) | Word::CmdSub(_) | Word::Arith(_));
            for s in self.expand_word(word)? {
                if split && s.contains(|c: char| ifs.contains(c)) {
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

    // ------------------------------------------------------------------
    // Heredoc body expansion
    // ------------------------------------------------------------------

    fn expand_heredoc_body(&mut self, body: &str) -> String {
        use crate::parser::parse;
        let mut result = String::with_capacity(body.len());
        let mut chars = body.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '$' {
                result.push(c);
                continue;
            }
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
                    result.push_str(&self.expand_var(&var));
                }
                Some('(') => {
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
                    if let Ok(program) = parse(&cmd_src)
                        && let Ok(s) = self.expand_cmd_sub(&program.into_command())
                    {
                        result.push_str(&s);
                    }
                }
                Some(c2) if c2.is_ascii_alphanumeric() || c2 == '_' || "@*#?-$!".contains(c2) => {
                    if "@*#?-$!".contains(c2) {
                        chars.next();
                        result.push_str(&self.expand_var(&c2.to_string()));
                    } else {
                        while matches!(chars.peek(), Some(ch) if ch.is_ascii_alphanumeric() || *ch == '_')
                        {
                            var.push(chars.next().unwrap());
                        }
                        result.push_str(&self.expand_var(&var));
                    }
                }
                _ => result.push('$'),
            }
        }
        if !result.ends_with('\n') {
            result.push('\n');
        }
        result
    }
}

// ---------------------------------------------------------------------------
// Heredoc pipe writer
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// execvp: PATH-searching exec
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn resolve_alias(env: &Env, raw: &str, rest: &[String]) -> (String, Vec<String>) {
    env.get_alias(raw).map_or_else(
        || (raw.to_owned(), rest.to_vec()),
        |alias_val| {
            let mut parts: Vec<String> = alias_val.split_whitespace().map(String::from).collect();
            let name = if parts.is_empty() {
                raw.to_owned()
            } else {
                parts.remove(0)
            };
            parts.extend_from_slice(rest);
            (name, parts)
        },
    )
}

pub fn is_assignment(word: &str) -> bool {
    let Some(eq) = word.find('=') else {
        return false;
    };
    let name = &word[..eq];
    !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

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
