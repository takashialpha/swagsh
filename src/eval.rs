use std::fmt;

use anyhow::{Error, Result};
use rustix::process::{Pid, Signal, getpid, setpgid};
use rustix::termios::{OptionalActions, Termios, tcgetattr, tcsetattr, tcsetpgrp};

use crate::ast::{Command, Program, Redirect, SimpleCmd, Word};
use crate::builtins::flow::ExitRequest;
use crate::builtins::{self, BuiltinFailed, BuiltinFn};
use crate::env::Env;
use crate::errfmt::emit;
use crate::fd::{restore_fds, save_fds, write_raw};
use crate::jobs::{ExitStatus, JobTable};
use crate::parser::{ParseError, Parser};
use crate::signal::{sig_interrupt_action, take_interrupted, take_pending_traps};
use crate::sys::{SigHandler, sigaction};

mod compound;
mod exec;
mod expand;

// ---------------------------------------------------------------------------
// Control-flow signals: propagate through the call stack via anyhow::Error.
// ---------------------------------------------------------------------------

/// The `u32` is how many enclosing loops remain to unwind through.
///
/// `break 2` starts as `Break(2)`, and each loop that catches it either
/// absorbs it (count was 1) or exits and re-raises `Break(count - 1)` to the
/// next loop out. `continue N` follows the same decrement, except the loop
/// that finally absorbs it re-runs its condition instead of exiting.
#[derive(Debug)]
pub enum LoopSignal {
    Break(u32),
    Continue(u32),
}

impl fmt::Display for LoopSignal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Break(_) => f.write_str("break outside loop"),
            Self::Continue(_) => f.write_str("continue outside loop"),
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
/// last check (see `signal::take_interrupted`).
///
/// Unwinds through the same
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

#[must_use]
pub fn is_interrupted(e: &Error) -> bool {
    e.downcast_ref::<Interrupted>().is_some()
}

#[must_use]
pub fn is_break(e: &Error) -> bool {
    matches!(e.downcast_ref::<LoopSignal>(), Some(LoopSignal::Break(_)))
}

#[must_use]
pub fn is_continue(e: &Error) -> bool {
    matches!(
        e.downcast_ref::<LoopSignal>(),
        Some(LoopSignal::Continue(_))
    )
}

/// Levels left to unwind for a caught `break`/`continue`.
///
/// I.e. the `N` in `break N`/`continue N` minus however many enclosing loops
/// already passed it along. Only meaningful once `is_break`/`is_continue`
/// confirmed the variant, so callers that already checked can safely
/// default to `1`.
#[must_use]
pub fn loop_signal_level(e: &Error) -> u32 {
    match e.downcast_ref::<LoopSignal>() {
        Some(LoopSignal::Break(n) | LoopSignal::Continue(n)) => *n,
        None => 1,
    }
}

#[must_use]
pub fn is_return(e: &Error) -> bool {
    e.downcast_ref::<ReturnSignal>().is_some()
}

/// Catches a `return` propagated as an error at a function-call boundary and
/// turns it into that function's exit status, so `return` doesn't keep
/// unwinding past the function that should absorb it.
fn catch_return(result: Result<ExitStatus>) -> Result<ExitStatus> {
    match result {
        Err(e) if is_return(&e) => match e.downcast::<ReturnSignal>() {
            Ok(sig) => Ok(ExitStatus(sig.0)),
            Err(e) => Err(e),
        },
        other => other,
    }
}

// ---------------------------------------------------------------------------
// Shell: the shared execution context.
// ---------------------------------------------------------------------------

// Each bool is an independently-named, independently-documented piece of
// shell state (`interactive`, `errexit`, `xtrace`, ...), set one field at a
// time via `set`/`Shell::new`, never constructed positionally, so there's no
// risk of the "which bool means what" confusion this lint guards against.
#[allow(clippy::struct_excessive_bools)]
pub struct Shell {
    pub env: Env,
    pub jobs: JobTable,
    pub pgid: Pid,
    pub last_status: ExitStatus,
    pub interactive: bool,
    /// `$0`: the shell's name, the script's path, or the name given after a
    /// `-c` command string.
    pub arg0: String,
    sane_termios: Option<Termios>,
    /// Whether the cursor is at the start of a fresh terminal line, i.e.
    /// the last thing a builtin wrote to stdout ended in `\n`. Builtins
    /// write through [`Shell::out`], which keeps this up to date: rustyline
    /// clears the current line before redrawing its next prompt, which
    /// would otherwise silently erase that output before it's ever seen,
    /// since nothing separates it onto its own line first. The REPL loop
    /// bridges with a real newline when this is `false`.
    pub at_line_start: bool,
    /// The `$!` special parameter: pid of the last process in the most
    /// recently backgrounded pipeline. `None` (expands empty) until the
    /// first `&` command runs.
    pub last_bg_pid: Option<Pid>,
    /// How many `for`/`while`/`until` loops currently enclose execution,
    /// scoped per function call (see `run_function`). `break`/`continue`
    /// clamp their `N` argument to this so a level exceeding the actual
    /// nesting just unwinds everything instead of escaping as an error, and
    /// treat `0` as "not in a loop at all" (the `"only meaningful in a
    /// 'for', 'while', or 'until' loop"` case).
    pub loop_depth: u32,
    /// `getopts`'s position within the *current* `$OPTIND` word: how many
    /// of its option characters have already been consumed by prior calls
    /// (`0` means start that word fresh). `getopts` calls are one-option-
    /// per-call, so this has to persist somewhere between them; unlike
    /// `$OPTIND` itself, this part isn't exposed as a shell variable, so
    /// it lives on `Shell` instead. See `builtins::getopts`.
    pub getopts_state: GetoptsState,
    /// `set -e`/`+e`: exit the shell when a command that isn't otherwise
    /// exempt (see `Shell::check_errexit`) fails.
    pub errexit: bool,
    /// `set -x`/`+x`: print each simple command to stderr, expanded, before
    /// running it.
    pub xtrace: bool,
    /// `set -u`/`+u`: expanding an unset (not just empty) variable is an
    /// error instead of silently becoming `""`.
    pub nounset: bool,
    /// `set -C`: `>` refuses to overwrite an existing regular file.
    pub noclobber: bool,
    /// `set -f`: no pathname expansion.
    pub noglob: bool,
    /// `set -n`: read commands without running them (ignored when
    /// interactive, where it would make the shell unusable).
    pub noexec: bool,
    /// `set -v`: echo each command's source to stderr as it is read.
    pub verbose: bool,
    /// `set -m`: job control. On by default only for interactive shells.
    /// Pipelines get their own process group and the terminal, and `fg`/`bg`
    /// work.
    pub monitor: bool,
    /// Commands entered interactively, oldest first, for `history` and `fc`.
    /// The REPL appends to it, and rebuilds the line editor's own copy when
    /// a builtin sets `history_changed`.
    pub history: Vec<String>,
    pub history_changed: bool,
    /// `pushd`'s saved directories, most recent first; `dirs` shows `$PWD`
    /// in front of them.
    pub dir_stack: Vec<String>,
    /// `trap` actions by signal number, with `0` for `EXIT`. An empty action
    /// means the signal is ignored.
    pub traps: std::collections::BTreeMap<i32, String>,
    /// A trap action is running, so pending traps wait until it finishes.
    in_trap: bool,
    /// How many `if`/`while`/`until` *condition* lists currently enclose
    /// execution: `errexit` doesn't apply inside one (a command's failure
    /// there is "explicitly tested", not a script-ending error), the same
    /// way `loop_depth` scopes `break`/`continue`. A counter, not a bool,
    /// since conditions can nest (`if while false; do :; done`).
    pub condition_depth: u32,
}

/// A command that does not parse, found by [`Shell::run_source`].
#[derive(Debug)]
pub struct SyntaxError {
    pub context: String,
    pub error: ParseError,
}

impl fmt::Display for SyntaxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.context, self.error)
    }
}

impl std::error::Error for SyntaxError {}

/// A builtin's write to standard output failed.
#[derive(Debug)]
pub struct WriteError(pub rustix::io::Errno);

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "write error: {}", crate::errfmt::strerror(self.0))
    }
}

impl std::error::Error for WriteError {}

#[derive(Default)]
pub struct GetoptsState {
    pub optind: i64,
    pub offset: usize,
}

impl Shell {
    #[must_use]
    pub fn new(env: Env, interactive: bool) -> Self {
        let pgid = getpid();
        let mut sane_termios = None;
        if interactive {
            let _ = setpgid(Some(pgid), Some(pgid));
            let _ = tcsetpgrp(std::io::stdin(), pgid);
            sane_termios = tcgetattr(std::io::stdin()).ok();
            // SAFETY: main shell process, single-threaded at startup.
            unsafe {
                let _ = sigaction(Signal::TTOU, SigHandler::Ignore);
                let _ = sigaction(Signal::TTIN, SigHandler::Ignore);
                let _ = sigaction(Signal::TSTP, SigHandler::Ignore);
                let _ = sigaction(Signal::INT, sig_interrupt_action());
            }
        }
        Self {
            env,
            jobs: JobTable::default(),
            pgid,
            last_status: ExitStatus::SUCCESS,
            interactive,
            arg0: std::env::args().next().unwrap_or_default(),
            sane_termios,
            at_line_start: true,
            last_bg_pid: None,
            loop_depth: 0,
            getopts_state: GetoptsState::default(),
            errexit: false,
            xtrace: false,
            nounset: false,
            noclobber: false,
            noglob: false,
            noexec: false,
            verbose: false,
            monitor: interactive,
            history: Vec::new(),
            history_changed: false,
            dir_stack: Vec::new(),
            traps: std::collections::BTreeMap::new(),
            in_trap: false,
            condition_depth: 0,
        }
    }

    /// `set -m`/`+m`. Turning job control on outside an interactive shell
    /// also does what `Shell::new` does for one: take a process group of
    /// our own and the terminal, and ignore the job-control stop signals.
    pub fn set_monitor(&mut self, enable: bool) {
        if enable && !self.monitor && !self.interactive {
            let _ = setpgid(Some(self.pgid), Some(self.pgid));
            let _ = tcsetpgrp(std::io::stdin(), self.pgid);
            // SAFETY: the shell is single-threaded.
            unsafe {
                let _ = sigaction(Signal::TTOU, SigHandler::Ignore);
                let _ = sigaction(Signal::TTIN, SigHandler::Ignore);
                let _ = sigaction(Signal::TSTP, SigHandler::Ignore);
            }
        }
        self.monitor = enable;
    }

    /// Exits the shell with `code`, running the `EXIT` trap first if one is
    /// set. The exit status stays `code` unless the trap itself calls
    /// `exit`.
    pub fn exit(&mut self, code: i32) -> ! {
        if let Some(action) = self.traps.remove(&0)
            && !action.is_empty()
        {
            self.last_status = ExitStatus(code);
            if let Err(e) = self.run_source(&action, "trap: ") {
                emit(e);
            }
        }
        std::process::exit(code);
    }

    /// Runs the action of every trapped signal that arrived since the last
    /// check. `$?` is the same afterwards as before.
    pub fn run_pending_traps(&mut self) {
        if self.in_trap {
            return;
        }
        let pending = take_pending_traps();
        if pending == 0 {
            return;
        }
        self.in_trap = true;
        let status = self.last_status;
        for sig in 1..64 {
            if pending & (1 << sig) == 0 {
                continue;
            }
            if let Some(action) = self.traps.get(&sig).filter(|a| !a.is_empty()).cloned() {
                match self.run_source(&action, "trap: ") {
                    Ok(_) => {}
                    Err(e) if is_break(&e) || is_continue(&e) || is_return(&e) => {}
                    Err(e) => match e.downcast_ref() {
                        Some(ExitRequest(code)) => self.exit(*code),
                        None => emit(e),
                    },
                }
            }
        }
        self.last_status = status;
        self.in_trap = false;
    }

    /// In a forked subshell: POSIX resets every trap that is not ignoring
    /// its signal back to the default.
    pub fn reset_traps_for_subshell(&mut self) {
        self.traps.retain(|&sig, action| {
            if action.is_empty() {
                return true;
            }
            if let Some(signal) = Signal::from_named_raw(sig) {
                // SAFETY: the subshell is single-threaded.
                let _ = unsafe { sigaction(signal, SigHandler::Default) };
            }
            false
        });
        self.interactive = false;
        self.monitor = false;
    }

    /// `$-`: the letters of the options currently on.
    #[must_use]
    pub fn option_flags(&self) -> String {
        [
            ('a', self.env.allexport),
            ('C', self.noclobber),
            ('e', self.errexit),
            ('f', self.noglob),
            ('i', self.interactive),
            ('m', self.monitor),
            ('n', self.noexec),
            ('u', self.nounset),
            ('v', self.verbose),
            ('x', self.xtrace),
        ]
        .iter()
        .filter(|(_, on)| *on)
        .map(|(c, _)| c)
        .collect()
    }

    /// Writes `text` to standard output for a builtin, recording where the
    /// cursor ends up (see [`Shell::note_stdout`]).
    ///
    /// Goes to descriptor 1 directly rather than through `print!`, which
    /// panics on a full disk and silently drops output when the descriptor is
    /// closed. Either failure is a [`WriteError`] here instead, which
    /// `run_builtin` reports as `NAME: write error: ...` with status 1.
    ///
    /// # Errors
    ///
    /// Returns a [`WriteError`] if the write fails.
    pub fn out(&mut self, text: &str) -> Result<()> {
        use std::io::Write as _;
        // Anything still buffered by `print!` elsewhere goes first.
        let _ = std::io::stdout().flush();
        let mut rest = text.as_bytes();
        while !rest.is_empty() {
            match write_raw(1, rest) {
                Ok(0) => return Err(WriteError(rustix::io::Errno::IO).into()),
                Ok(n) => rest = &rest[n..],
                Err(rustix::io::Errno::INTR) => {}
                Err(e) => return Err(WriteError(e).into()),
            }
        }
        self.note_stdout(text);
        Ok(())
    }

    /// [`Shell::out`] followed by a newline.
    ///
    /// # Errors
    ///
    /// Returns a [`WriteError`] if the write fails.
    pub fn outln(&mut self, text: &str) -> Result<()> {
        let mut line = String::with_capacity(text.len() + 1);
        line.push_str(text);
        line.push('\n');
        self.out(&line)
    }

    /// Records whether the text just written to stdout ended in a newline.
    /// [`Shell::out`] calls this itself; pass the exact string that was
    /// printed so the check is exact rather than assumed.
    pub fn note_stdout(&mut self, printed: &str) {
        if !printed.is_empty() {
            self.at_line_start = printed.ends_with('\n');
        }
    }

    /// `set -e`: exits the shell if `status` is a failure that isn't
    /// exempt. Exemptions: inside an `if`/`while`/`until` condition
    /// (`condition_depth > 0`, the caller's job to track), and a pipeline
    /// ending in `!` (`negated`) regardless of its resulting status, since
    /// negation counts as "explicitly tested" even when the negated result
    /// is still a failure (`! true` does not exit under `-e`, despite
    /// `! true` itself being status 1). Called once per `AndOrList` (a
    /// `&&`/`||` chain), on the last item actually executed: short-
    /// circuiting already means every earlier item's failure was
    /// "explicitly tested" by the operator that follows it, so only the
    /// chain's final outcome is a candidate to exit on.
    pub fn check_errexit(&mut self, status: ExitStatus, negated: bool) {
        if self.errexit && self.condition_depth == 0 && !negated && !status.is_success() {
            self.exit(status.0);
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

    /// Parses and runs `src` one complete command at a time, the way a
    /// script is read: an alias defined by one command applies from the
    /// next on, and a syntax error only stops the run once it is reached.
    /// `set -v` echoes each command's source to stderr as it is read.
    /// `context` prefixes a syntax error's message (`"eval: "`, a path, ...).
    ///
    /// # Errors
    ///
    /// Returns a [`SyntaxError`] for the first malformed command, or
    /// whatever running a command returns.
    pub fn run_source(&mut self, src: &str, context: &str) -> Result<ExitStatus> {
        let mut parser = Parser::new(src);
        let mut status = ExitStatus::SUCCESS;
        let mut echoed = 0;
        loop {
            let next = parser.next_command(Some(self.env.aliases()));
            if self.verbose {
                let end = parser.consumed_offset().max(echoed);
                eprint!("{}", &src[echoed..end]);
                echoed = end;
            }
            match next {
                Ok(Some(program)) => status = self.run_program(&program)?,
                Ok(None) => return Ok(status),
                Err(error) => {
                    return Err(SyntaxError {
                        context: context.to_owned(),
                        error,
                    }
                    .into());
                }
            }
        }
    }

    /// # Errors
    ///
    /// Returns an error if running any statement in `program` fails.
    pub fn run_program(&mut self, program: &Program) -> Result<ExitStatus> {
        let mut status = ExitStatus::SUCCESS;
        for aol in &program.body {
            status = self.run_and_or(aol)?;
        }
        Ok(status)
    }

    /// # Errors
    ///
    /// Returns an error if running `cmd` fails, or (via the same
    /// `anyhow::Error` control-flow path) to propagate `break`/`continue`/
    /// `return`/an interrupt.
    pub fn run_command(&mut self, cmd: &Command) -> Result<ExitStatus> {
        if self.interactive && take_interrupted() {
            return Err(Error::new(Interrupted));
        }
        self.run_pending_traps();
        if self.noexec && !self.interactive {
            return Ok(ExitStatus::SUCCESS);
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
            Command::Redirected(inner, redirects) => {
                self.with_redirects(redirects, false, |shell| shell.run_command(inner))
            }
        }
    }

    fn run_simple(&mut self, sc: &SimpleCmd) -> Result<ExitStatus> {
        let (assignments, resolved) = self.resolve_simple(sc)?;
        if self.xtrace {
            self.print_xtrace(&assignments, &resolved);
        }
        match resolved {
            Resolved::AssignOnly => {
                // Nothing to run (`resolve_simple` already set the vars):
                // apply redirects for their side effects, e.g. `> file`
                // still truncates it.
                self.with_redirects(&sc.redirects, false, |_| Ok(ExitStatus::SUCCESS))
            }
            Resolved::Builtin(f, name, args) => {
                let special = is_special_builtin(&name);
                self.run_builtin(sc, f, &name, &args, &assignments, special)
            }
            Resolved::Function(_, body, args) => self.run_function(sc, &body, &args, &assignments),
            Resolved::External(words) => self.run_external(sc, &words, &assignments),
        }
    }

    /// `set -x`: prints the fully-expanded command about to run, prefixed
    /// with `$PS4` (defaulting to `"+ "`), before it runs. Values are
    /// space-joined without re-quoting: simple, not always round-trippable,
    /// but that's the conventional shell `-x` trace format.
    fn print_xtrace(&self, assignments: &[String], resolved: &Resolved) {
        let mut trace: Vec<&str> = assignments.iter().map(String::as_str).collect();
        match resolved {
            Resolved::AssignOnly => {}
            Resolved::Builtin(_, name, args) | Resolved::Function(name, _, args) => {
                trace.push(name);
                trace.extend(args.iter().map(String::as_str));
            }
            Resolved::External(words) => trace.extend(words.iter().map(String::as_str)),
        }
        if !trace.is_empty() {
            let ps4 = self.env.get("PS4").unwrap_or_else(|| "+ ".to_owned());
            eprintln!("{ps4}{}", trace.join(" "));
        }
    }

    /// Splits off any leading `VAR=val` assignment words (recognized as
    /// written, before expansion), expands the rest, and looks up what kind
    /// of command the name refers to. Aliases were already expanded by the
    /// parser.
    ///
    /// Shared by `run_simple` (which may run the result in-process or fork
    /// once for an external command) and `exec::exec_simple_in_child`,
    /// which is already inside a forked pipeline-stage child and just runs
    /// the resolved command to completion before the child exits. Keeping
    /// this resolution logic in one place means both paths agree on what
    /// counts as a builtin, a function, or an external command.
    fn resolve_simple(&mut self, sc: &SimpleCmd) -> Result<(Vec<String>, Resolved)> {
        let assign_count = sc
            .words
            .iter()
            .take_while(|w| is_assignment_word(w))
            .count();
        let (assign_words, cmd_words) = sc.words.split_at(assign_count);
        let mut cmd_words = self.expand_command_words(cmd_words)?.into_iter();

        let Some(name) = cmd_words.next() else {
            // Performed in order, so `a=1 b=$a` sees the new `a`.
            let mut assignments = Vec::with_capacity(assign_words.len());
            for word in assign_words {
                let assign = self.expand_assignment(word)?;
                let (name, value) = split_assignment(&assign);
                self.env.assign(name, value)?;
                assignments.push(assign);
            }
            return Ok((assignments, Resolved::AssignOnly));
        };
        // Each prefix assignment is visible while the next one expands, as
        // in `HOME=/h dir=~ cmd`, then undone until the command applies them.
        let mut assignments = Vec::with_capacity(assign_words.len());
        let mut saved = Vec::new();
        let mut result = Ok(());
        for (i, word) in assign_words.iter().enumerate() {
            match self.expand_assignment(word) {
                Ok(assign) => {
                    let name = split_assignment(&assign).0;
                    if self.env.is_readonly(name) {
                        result = Err(crate::env::ReadonlyError(name.to_owned()).into());
                        break;
                    }
                    if i + 1 < assign_words.len() {
                        let (name, value) = split_assignment(&assign);
                        saved.push((name.to_owned(), self.env.get(name)));
                        self.env.set(name, value);
                    }
                    assignments.push(assign);
                }
                Err(e) => {
                    result = Err(e);
                    break;
                }
            }
        }
        saved.reverse();
        self.env.restore_vars(saved);
        result?;

        let args: Vec<String> = cmd_words.collect();

        // POSIX search order: special builtins, then functions, then the
        // other builtins, so a function can wrap `cd` and the like.
        let builtin = builtins::lookup_builtin(name.as_str());
        if let Some(f) = builtin.filter(|_| is_special_builtin(&name)) {
            return Ok((assignments, Resolved::Builtin(f, name, args)));
        }
        if let Some(body) = self.env.get_function(&name) {
            return Ok((assignments, Resolved::Function(name, body, args)));
        }
        if let Some(f) = builtin {
            return Ok((assignments, Resolved::Builtin(f, name, args)));
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
            let assign_count = sc
                .words
                .iter()
                .take_while(|w| is_assignment_word(w))
                .count();
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
                self.env.assign(k, v)?;
            }
            vec![]
        } else {
            self.apply_temp_assignments(assignments)
        };

        // `exec`'s redirect handling can't go through the generic
        // `with_redirects` path below: a bare `exec` (no COMMAND) applies
        // its redirects permanently to this shell instead of scoping them
        // to one call. See `run_exec_builtin`'s doc comment.
        if name == "exec" {
            return self.run_exec_builtin(&arg_refs, &sc.redirects);
        }

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
        let result =
            self.with_redirects(&sc.redirects, special, |shell| match f(shell, &arg_refs) {
                Err(e) if e.is::<WriteError>() => {
                    emit(format!("{name}: {e}"));
                    Ok(ExitStatus::FAILURE)
                }
                Err(e)
                    if !is_break(&e)
                        && !is_continue(&e)
                        && !is_return(&e)
                        && !is_interrupted(&e)
                        && !e.is::<ExitRequest>() =>
                {
                    let status = e.downcast_ref::<BuiltinFailed>().map_or_else(
                        || {
                            emit(&e);
                            ExitStatus::FAILURE
                        },
                        |failed| failed.0,
                    );
                    // POSIX: a special builtin's error ends a
                    // non-interactive shell.
                    if special && !shell.interactive {
                        shell.exit(status.0);
                    }
                    Ok(status)
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

        let old_args = self.env.replace_positional_args(args.to_vec());
        // `break`/`continue` don't reach through a function call into a
        // loop the caller happens to be inside: each function body has its
        // own independent loop nesting, so hide the caller's depth for the
        // duration of this call.
        let outer_loop_depth = std::mem::take(&mut self.loop_depth);
        self.env.push_scope();
        let result = self.with_redirects(&sc.redirects, false, |shell| shell.run_command(body));
        self.env.pop_scope();
        self.loop_depth = outer_loop_depth;
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

    /// Saves fds 0/1/2 and every fd a redirect targets, applies `redirects`,
    /// runs `f`, then restores the original fds regardless of outcome. If
    /// applying the redirects fails, `f` doesn't run and that error is
    /// returned.
    ///
    /// Skips the save/restore dance entirely when there are no redirects to
    /// apply: `run_builtin`/`run_function` call this unconditionally for
    /// every invocation, and the no-redirect case (`f() { ...; }`, `[ ... ]`,
    /// `:`, ...) is by far the common one in loops and functions. Each
    /// save/restore round-trip is 3 `dup`s out plus 3 `dup2`s and 3
    /// `close`s back in, 9 syscalls that were previously paid on every
    /// builtin and function call regardless of whether there was anything
    /// to restore.
    ///
    /// A redirect that fails is reported and gives status 1 without running
    /// `f`, except for a `special` builtin, where POSIX makes it fatal and the
    /// error is returned instead.
    fn with_redirects(
        &mut self,
        redirects: &[Redirect],
        special: bool,
        f: impl FnOnce(&mut Self) -> Result<ExitStatus>,
    ) -> Result<ExitStatus> {
        if redirects.is_empty() {
            return f(self);
        }
        let mut fds = vec![0, 1, 2];
        fds.extend(redirects.iter().map(|r| r.fd).filter(|&fd| fd > 2));
        fds.sort_unstable();
        fds.dedup();
        let saved_fds = save_fds(&fds)?;
        let result = match self.apply_redirects(redirects) {
            Ok(()) => f(self),
            Err(e) if special => Err(e),
            Err(e) => {
                let _ = restore_fds(saved_fds);
                emit(e);
                return Ok(ExitStatus::FAILURE);
            }
        };
        let _ = restore_fds(saved_fds);
        result
    }
}

/// What a `SimpleCmd` resolves to, per `Shell::resolve_simple`.
enum Resolved {
    /// No command words after expansion (e.g. a bare `FOO=bar`).
    AssignOnly,
    Builtin(BuiltinFn, String, Vec<String>),
    /// The function's own name, kept around for `xtrace` display (`run_function`
    /// itself doesn't need it: a function's `$0` stays whatever the caller's
    /// was, unlike positional args) alongside its body and arguments.
    Function(String, std::rc::Rc<Command>, Vec<String>),
    /// `name` followed by its arguments.
    External(Vec<String>),
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Whether `word`, as written, is a `NAME=value` assignment. Decided before
/// expansion, so `$v` holding `a=b` is an ordinary word.
pub(crate) fn is_assignment_word(word: &Word) -> bool {
    match word {
        Word::Literal(s) => is_assignment(s),
        Word::Compound(parts) => {
            matches!(parts.first(), Some(Word::Literal(s)) if is_assignment(s))
        }
        _ => false,
    }
}

#[must_use]
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
/// Falls back to treating the whole word as the name if that invariant is
/// ever violated, rather than panicking a live shell over an internal bug.
fn split_assignment(word: &str) -> (&str, &str) {
    word.split_once('=').unwrap_or((word, ""))
}

/// The POSIX special builtins, which are found before functions and whose
/// errors end a non-interactive shell.
#[must_use]
pub fn is_special_builtin(name: &str) -> bool {
    matches!(
        name,
        "." | ":"
            | "break"
            | "continue"
            | "eval"
            | "exec"
            | "exit"
            | "export"
            | "readonly"
            | "return"
            | "set"
            | "shift"
            | "source"
            | "times"
            | "trap"
            | "unset"
    )
}
