use std::fmt;

use anyhow::{Error, Result};
use rustix::process::{Pid, Signal, getpid, setpgid};
use rustix::runtime::kernel_sigaction;
use rustix::termios::tcsetpgrp;

use crate::ast::{Command, Program, SimpleCmd};
use crate::builtins::{self, BuiltinFn};
use crate::env::Env;
use crate::fd::{restore_fds, save_fds};
use crate::jobs::{ExitStatus, JobTable};
use crate::signal::sig_ign_action;

mod compound;
mod exec;
mod expand;

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

    pub fn run_program(&mut self, program: &Program) -> Result<ExitStatus> {
        let mut status = ExitStatus::SUCCESS;
        for aol in &program.body {
            status = self.run_and_or(aol)?;
        }
        Ok(status)
    }

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
