use std::fmt;

use anyhow::{Error, Result};
use rustix::process::{Pid, Signal, getpid, setpgid};
use rustix::runtime::kernel_sigaction;
use rustix::termios::{OptionalActions, Termios, tcgetattr, tcsetattr, tcsetpgrp};

use crate::ast::{Command, Program, SimpleCmd};
use crate::builtins::{self, BuiltinFn};
use crate::env::Env;
use crate::errfmt::emit;
use crate::fd::{restore_fds, save_fds};
use crate::jobs::{ExitStatus, JobTable};
use crate::signal::{sig_ign_action, sig_interrupt_action, take_interrupted};

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

/// Raised at the top of `run_command` when `SIGINT` has arrived since the
/// last check (see `signal::take_interrupted`). Unwinds through the same
/// `anyhow::Error` control-flow path as `break`/`continue`/`return` so a
/// `^C` mid-loop aborts the current top-level command instead of running
/// forever: there's no forked child for the OS to deliver a fatal default
/// `SIGINT` to in that case, since builtins/functions/loops all run
/// in-process.
#[derive(Debug)]
pub struct Interrupted;

impl fmt::Display for Interrupted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("interrupted")
    }
}

impl std::error::Error for Interrupted {}

pub fn is_interrupted(e: &Error) -> bool {
    e.downcast_ref::<Interrupted>().is_some()
}

pub fn is_break(e: &Error) -> bool {
    matches!(e.downcast_ref::<LoopSignal>(), Some(LoopSignal::Break))
}

pub fn is_continue(e: &Error) -> bool {
    matches!(e.downcast_ref::<LoopSignal>(), Some(LoopSignal::Continue))
}

pub fn is_return(e: &Error) -> bool {
    e.downcast_ref::<ReturnSignal>().is_some()
}

/// Catches a `return` propagated as an error at a function-call boundary and
/// turns it into that function's exit status, so `return` doesn't keep
/// unwinding past the function that should absorb it.
fn catch_return(result: Result<ExitStatus>) -> Result<ExitStatus> {
    match result {
        Err(e) if is_return(&e) => Ok(ExitStatus(e.downcast::<ReturnSignal>().unwrap().0)),
        other => other,
    }
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
    sane_termios: Option<Termios>,
    /// Whether the cursor is at the start of a fresh terminal line, i.e.
    /// the last thing a builtin wrote to stdout ended in `\n`. Builtins
    /// that can print without a trailing newline (`echo -n`, `printf`, ...)
    /// must update this via [`Shell::note_stdout`] after writing: rustyline
    /// clears the current line before redrawing its next prompt, which
    /// would otherwise silently erase that output before it's ever seen,
    /// since nothing separates it onto its own line first. The REPL loop
    /// bridges with a real newline when this is `false`.
    pub at_line_start: bool,
}

impl Shell {
    pub fn new(env: Env, interactive: bool) -> Self {
        let pgid = getpid();
        let mut sane_termios = None;
        if interactive {
            let _ = setpgid(Some(pgid), Some(pgid));
            let _ = tcsetpgrp(std::io::stdin(), pgid);
            sane_termios = tcgetattr(std::io::stdin()).ok();
            // SAFETY: main shell process, single-threaded at startup.
            unsafe {
                let ign = sig_ign_action();
                let _ = kernel_sigaction(Signal::TTOU, Some(ign.clone()));
                let _ = kernel_sigaction(Signal::TTIN, Some(ign.clone()));
                let _ = kernel_sigaction(Signal::TSTP, Some(ign));
                let _ = kernel_sigaction(Signal::INT, Some(sig_interrupt_action()));
            }
        }
        Self {
            env,
            jobs: JobTable::default(),
            pgid,
            last_status: ExitStatus::SUCCESS,
            interactive,
            sane_termios,
            at_line_start: true,
        }
    }

    /// Records whether the text a builtin just wrote to stdout ended in a
    /// newline. Pass the exact string that was printed (or `"\n"` for a
    /// `println!` whose payload doesn't matter) so the check is exact
    /// rather than assumed.
    pub fn note_stdout(&mut self, printed: &str) {
        if !printed.is_empty() {
            self.at_line_start = printed.ends_with('\n');
        }
    }

    /// Reasserts the shell's own known-good terminal modes after reclaiming
    /// the foreground process group from a child. A foreground job that
    /// disables echo (a password prompt, `su`/`sudo`'s pty relay, ...) and
    /// then dies to a signal before undoing that (e.g. `^C` mid-prompt)
    /// otherwise leaves the terminal stuck in that state for every command
    /// typed afterwards, us included.
    pub fn restore_terminal(&self) {
        if let Some(t) = &self.sane_termios {
            let _ = tcsetattr(std::io::stdin(), OptionalActions::Drain, t);
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
        if self.interactive && take_interrupted() {
            return Err(Error::new(Interrupted));
        }
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
        let (assignments, resolved) = self.resolve_simple(sc)?;
        match resolved {
            Resolved::AssignOnly => {
                // Nothing to run: apply redirects (for their side effects,
                // e.g. `> file` still truncates it), then set the vars.
                self.with_redirects(sc, |_| Ok(()))?;
                for assign in &assignments {
                    let (name, value) = split_assignment(assign);
                    self.env.set(name, value);
                }
                Ok(ExitStatus::SUCCESS)
            }
            Resolved::Builtin(f, name, args) => {
                let special = is_special_builtin(&name);
                self.run_builtin(sc, f, &name, &args, &assignments, special)
            }
            Resolved::Function(body, args) => self.run_function(sc, &body, &args, &assignments),
            Resolved::External(words) => self.run_external(sc, &words, &assignments),
        }
    }

    /// Expands `sc.words`, splits off any leading `VAR=val` assignment
    /// prefix, resolves alias substitution, and looks up what kind of
    /// command the (possibly alias-rewritten) name refers to.
    ///
    /// Shared by `run_simple` (which may run the result in-process or fork
    /// once for an external command) and `exec::exec_simple_in_child`,
    /// which is already inside a forked pipeline-stage child and just runs
    /// the resolved command to completion before the child exits. Keeping
    /// this resolution logic in one place means both paths agree on what
    /// counts as a builtin, a function, or an external command.
    fn resolve_simple(&mut self, sc: &SimpleCmd) -> Result<(Vec<String>, Resolved)> {
        let words = self.expand_words(&sc.words)?;
        let assign_count = words.iter().take_while(|w| is_assignment(w)).count();
        let (assignments, cmd_words) = words.split_at(assign_count);
        let assignments = assignments.to_vec();

        if cmd_words.is_empty() {
            return Ok((assignments, Resolved::AssignOnly));
        }

        let (name, args) = resolve_alias(&self.env, &cmd_words[0], &cmd_words[1..]);

        if let Some(f) = builtins::lookup_builtin(name.as_str()) {
            return Ok((assignments, Resolved::Builtin(f, name, args)));
        }
        if let Some(body) = self.env.get_function(&name) {
            return Ok((assignments, Resolved::Function(body, args)));
        }

        let mut full_words = vec![name];
        full_words.extend(args);
        Ok((assignments, Resolved::External(full_words)))
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
                let (k, v) = split_assignment(a);
                self.env.set(k, v);
            }
            vec![]
        } else {
            self.apply_temp_assignments(assignments)
        };

        // A builtin's own error (bad flag, ENOENT, ...) is a normal command
        // failure: it should set `$?` and let the script go on to the next
        // statement, the same as an external command exiting non-zero would.
        // `break`/`continue`/`return`/`^C` are the exception: those builtins
        // deliberately raise this same `Result<ExitStatus>` channel to signal
        // control flow, and must keep unwinding rather than being absorbed
        // here. Without this, any failing builtin (e.g. `cd` into a missing
        // directory) would `?` all the way out and abort everything after it
        // in the current script/line, not just that one command.
        //
        // This has to happen *inside* the `with_redirects` closure, while
        // any `2>...` on this command is still in effect: `with_redirects`
        // restores the original fds as soon as the closure returns, so
        // catching the error out here would `emit` it to the shell's real
        // stderr regardless of what the command's own redirects said.
        let result = self.with_redirects(sc, |shell| match f(shell, &arg_refs) {
            Err(e)
                if !is_break(&e) && !is_continue(&e) && !is_return(&e) && !is_interrupted(&e) =>
            {
                emit(e);
                Ok(ExitStatus::FAILURE)
            }
            other => other,
        });
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
        let saved_vars = self.apply_temp_assignments(assignments);

        let old_args = self.env.positional_args().to_vec();
        self.env.set_positional_args(args.to_vec());
        let result = self.with_redirects(sc, |shell| shell.run_command(body));
        self.env.set_positional_args(old_args);
        self.env.restore_vars(saved_vars);

        catch_return(result)
    }

    /// Sets each `VAR=val` word in the environment, returning the previous
    /// values so `Env::restore_vars` can undo them once the command the
    /// assignments were scoped to has finished (POSIX: a `VAR=val cmd`
    /// prefix only applies for the duration of `cmd`).
    fn apply_temp_assignments(&mut self, assignments: &[String]) -> Vec<(String, Option<String>)> {
        assignments
            .iter()
            .map(|a| {
                let (k, v) = split_assignment(a);
                let old = self.env.get(k);
                self.env.set(k, v);
                (k.to_owned(), old)
            })
            .collect()
    }

    /// Saves fds 0/1/2, applies `sc.redirects`, runs `f`, then restores the
    /// original fds regardless of outcome. If applying the redirects fails,
    /// `f` doesn't run and that error is returned.
    ///
    /// Skips the save/restore dance entirely when there are no redirects to
    /// apply: `run_builtin`/`run_function` call this unconditionally for
    /// every invocation, and the no-redirect case (`f() { ...; }`, `[ ... ]`,
    /// `:`, ...) is by far the common one in loops and functions. Each
    /// save/restore round-trip is 3 `dup`s out plus 3 `dup2`s and 3
    /// `close`s back in, 9 syscalls that were previously paid on every
    /// builtin and function call regardless of whether there was anything
    /// to restore.
    fn with_redirects<T>(
        &mut self,
        sc: &SimpleCmd,
        f: impl FnOnce(&mut Self) -> Result<T>,
    ) -> Result<T> {
        if sc.redirects.is_empty() {
            return f(self);
        }
        let saved_fds = save_fds(&[0, 1, 2])?;
        let result = self.apply_redirects(&sc.redirects).and_then(|()| f(self));
        let _ = restore_fds(saved_fds);
        result
    }
}

/// What a `SimpleCmd` resolves to, per `Shell::resolve_simple`.
enum Resolved {
    /// No command words after expansion (e.g. a bare `FOO=bar`).
    AssignOnly,
    Builtin(BuiltinFn, String, Vec<String>),
    Function(std::rc::Rc<Command>, Vec<String>),
    /// `name` followed by its arguments.
    External(Vec<String>),
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

/// Splits a word already validated by `is_assignment` into `(name, value)`.
fn split_assignment(word: &str) -> (&str, &str) {
    word.split_once('=')
        .expect("is_assignment guarantees `=` is present")
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
