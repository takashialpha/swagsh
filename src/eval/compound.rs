use crate::sys::{Fork, fork};
use std::fmt::Write as _;

use anyhow::Result;

use crate::ast::{
    AndOrList, AndOrOp, ArithForClause, CaseArm, CaseClause, CaseEnd, ForClause, GroupCmd,
    IfClause, WhileClause,
};
use crate::errfmt::emit;
use crate::expand::glob_match_case;
use crate::jobs::ExitStatus;
use crate::signal::restore_child_signals;

use super::{LoopSignal, Shell, is_break, is_continue, is_return, loop_signal_level};

impl Shell {
    pub(super) fn run_and_or(&mut self, aol: &AndOrList) -> Result<ExitStatus> {
        let items = &aol.items;
        // The parser never emits an empty `items`, but `AndOrList` is an
        // ordinary struct with public fields, so an empty one would reach
        // the `items.len() - 1` below and underflow instead of harmlessly
        // doing nothing.
        let Some(last_index) = items.len().checked_sub(1) else {
            return Ok(ExitStatus::SUCCESS);
        };
        let mut status = ExitStatus::SUCCESS;
        let mut last_negated = false;
        let mut last_executed = 0;
        let mut i = 0;
        while i < items.len() {
            let is_last = i == last_index;
            // POSIX: `set -e` is off for every command of an and-or list
            // but the last, and for a `!` pipeline, down through any
            // function they call.
            let tested = !is_last || items[i].command.negated;
            if tested {
                self.condition_depth += 1;
            }
            let result = if aol.is_async && is_last {
                self.run_pipeline_async(&items[i].command)
            } else {
                self.run_pipeline(&items[i].command)
            };
            if tested {
                self.condition_depth -= 1;
            }
            status = result?;
            last_negated = items[i].command.negated;
            last_executed = i;
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
        // `set -e` only applies when the chain's syntactically *last* item
        // is the one that actually ran and produced `status`: e.g.
        // `false && true` never runs `true` at all (short-circuited), so
        // even though `false` failed and `status` reflects that, this
        // doesn't exit here since the command that failed wasn't the
        // list's last one. Every earlier item's failure is likewise
        // "explicitly tested" by the `&&`/`||` that follows it, whether or
        // not the rest of the chain ran.
        if last_executed == last_index {
            self.check_errexit(status, last_negated);
        }
        Ok(status)
    }

    /// # Errors
    ///
    /// Returns an error if running any statement in `list` fails.
    pub fn run_list(&mut self, list: &[AndOrList]) -> Result<ExitStatus> {
        let mut status = ExitStatus::SUCCESS;
        for aol in list {
            status = self.run_and_or(aol)?;
        }
        Ok(status)
    }

    pub(super) fn run_if(&mut self, ic: &IfClause) -> Result<ExitStatus> {
        if self.run_condition(&ic.condition)?.is_success() {
            return self.run_list(&ic.then_body);
        }
        for (elif_cond, elif_body) in &ic.elif_clauses {
            if self.run_condition(elif_cond)?.is_success() {
                return self.run_list(elif_body);
            }
        }
        if let Some(else_body) = &ic.else_body {
            return self.run_list(else_body);
        }
        Ok(ExitStatus::SUCCESS)
    }

    /// Runs an `if`/`elif`/`while`/`until` condition list with `set -e`
    /// suppressed for its whole duration (`condition_depth`), not just its
    /// last statement: e.g. `if false; true; then :; fi` under `-e`
    /// doesn't exit even though `false` isn't the part `if` actually
    /// tests, so the exemption covers every statement in the list, not
    /// only the one whose status matters.
    fn run_condition(&mut self, condition: &[AndOrList]) -> Result<ExitStatus> {
        self.condition_depth += 1;
        let result = self.run_list(condition);
        self.condition_depth -= 1;
        result
    }

    pub(super) fn run_for(&mut self, fc: &ForClause) -> Result<ExitStatus> {
        // `expand_words`, not a flatten of per-word `expand_word`: a `for`
        // list is a word list exactly like a command's arguments, so an
        // unquoted `$v` or `$(...)` in it undergoes IFS field splitting,
        // which only `expand_words` does.
        let items = self.expand_words(&fc.items)?;

        self.loop_depth += 1;
        let mut status = ExitStatus::SUCCESS;
        let mut propagate = None;
        for item in items {
            self.env.assign(&fc.var, item)?;
            match self.run_list(&fc.body) {
                Ok(s) => status = s,
                Err(e) if is_break(&e) => {
                    status = ExitStatus::SUCCESS;
                    let n = loop_signal_level(&e);
                    if n > 1 {
                        propagate = Some(LoopSignal::Break(n - 1));
                    }
                    break;
                }
                Err(e) if is_continue(&e) => {
                    let n = loop_signal_level(&e);
                    if n > 1 {
                        propagate = Some(LoopSignal::Continue(n - 1));
                        break;
                    }
                }
                Err(e) => {
                    self.loop_depth -= 1;
                    return Err(e);
                }
            }
        }
        self.loop_depth -= 1;
        propagate.map_or(Ok(status), |sig| Err(anyhow::Error::new(sig)))
    }

    /// `(( expression ))`: status 0 when the expression is not zero, 1
    /// when it is or cannot be evaluated.
    pub(super) fn run_arith(&mut self, expr: &str) -> ExitStatus {
        match self.eval_arith_value(expr) {
            Ok(0) => ExitStatus::FAILURE,
            Ok(_) => ExitStatus::SUCCESS,
            Err(e) => {
                emit(e);
                ExitStatus::FAILURE
            }
        }
    }

    pub(super) fn run_arith_for(&mut self, fc: &ArithForClause) -> Result<ExitStatus> {
        if !fc.init.trim().is_empty() {
            self.eval_arith_value(&fc.init)?;
        }
        self.loop_depth += 1;
        let mut status = ExitStatus::SUCCESS;
        let mut propagate = None;
        let result = loop {
            if !fc.condition.trim().is_empty() {
                match self.eval_arith_value(&fc.condition) {
                    Ok(0) => break Ok(()),
                    Ok(_) => {}
                    Err(e) => break Err(e),
                }
            }
            match self.run_list(&fc.body) {
                Ok(s) => status = s,
                Err(e) if is_break(&e) => {
                    status = ExitStatus::SUCCESS;
                    let n = loop_signal_level(&e);
                    if n > 1 {
                        propagate = Some(LoopSignal::Break(n - 1));
                    }
                    break Ok(());
                }
                Err(e) if is_continue(&e) => {
                    let n = loop_signal_level(&e);
                    if n > 1 {
                        propagate = Some(LoopSignal::Continue(n - 1));
                        break Ok(());
                    }
                }
                Err(e) => break Err(e),
            }
            if !fc.step.trim().is_empty()
                && let Err(e) = self.eval_arith_value(&fc.step)
            {
                break Err(e);
            }
        };
        self.loop_depth -= 1;
        result?;
        propagate.map_or(Ok(status), |sig| Err(anyhow::Error::new(sig)))
    }

    /// `select`: shows the items as a numbered menu on standard error, reads
    /// a line, sets `REPLY` to it and the variable to the item it numbers
    /// (empty for anything else), and runs the body; an empty line shows
    /// the menu again, and the end of input ends the loop.
    pub(super) fn run_select(&mut self, fc: &ForClause) -> Result<ExitStatus> {
        let items = self.expand_words(&fc.items)?;
        if items.is_empty() {
            return Ok(ExitStatus::SUCCESS);
        }
        let width = items.len().to_string().len();
        let mut menu = String::new();
        for (i, item) in items.iter().enumerate() {
            let _ = writeln!(menu, "{:>width$}) {item}", i + 1);
        }
        self.loop_depth += 1;
        let mut status = ExitStatus::SUCCESS;
        let mut propagate = None;
        let mut show_menu = true;
        let result = loop {
            if show_menu {
                eprint!("{menu}");
            }
            let ps3 = self.env.get("PS3").unwrap_or_else(|| "#? ".to_owned());
            eprint!("{ps3}");
            let (line, found) = match crate::builtins::read_line(0) {
                Ok(read) => read,
                Err(e) => break Err(e.into()),
            };
            if !found && line.is_empty() {
                eprintln!();
                status = ExitStatus::FAILURE;
                break Ok(());
            }
            let line = String::from_utf8_lossy(&line).into_owned();
            show_menu = line.trim().is_empty();
            if show_menu {
                continue;
            }
            let chosen = line
                .trim()
                .parse::<usize>()
                .ok()
                .and_then(|n| n.checked_sub(1))
                .and_then(|i| items.get(i))
                .cloned()
                .unwrap_or_default();
            self.env.set("REPLY", line);
            if let Err(e) = self.env.assign(&fc.var, chosen) {
                break Err(e.into());
            }
            match self.run_list(&fc.body) {
                Ok(s) => status = s,
                Err(e) if is_break(&e) => {
                    status = ExitStatus::SUCCESS;
                    let n = loop_signal_level(&e);
                    if n > 1 {
                        propagate = Some(LoopSignal::Break(n - 1));
                    }
                    break Ok(());
                }
                Err(e) if is_continue(&e) => {
                    let n = loop_signal_level(&e);
                    if n > 1 {
                        propagate = Some(LoopSignal::Continue(n - 1));
                        break Ok(());
                    }
                }
                Err(e) => break Err(e),
            }
        };
        self.loop_depth -= 1;
        result?;
        propagate.map_or(Ok(status), |sig| Err(anyhow::Error::new(sig)))
    }

    pub(super) fn run_while(&mut self, wc: &WhileClause) -> Result<ExitStatus> {
        self.loop_depth += 1;
        let mut status = ExitStatus::SUCCESS;
        let mut propagate = None;
        loop {
            let cond = match self.run_condition(&wc.condition) {
                Ok(c) => c,
                Err(e) => {
                    self.loop_depth -= 1;
                    return Err(e);
                }
            };
            if wc.until == cond.is_success() {
                break;
            }
            match self.run_list(&wc.body) {
                Ok(s) => status = s,
                Err(e) if is_break(&e) => {
                    status = ExitStatus::SUCCESS;
                    let n = loop_signal_level(&e);
                    if n > 1 {
                        propagate = Some(LoopSignal::Break(n - 1));
                    }
                    break;
                }
                Err(e) if is_continue(&e) => {
                    let n = loop_signal_level(&e);
                    if n > 1 {
                        propagate = Some(LoopSignal::Continue(n - 1));
                        break;
                    }
                }
                Err(e) => {
                    self.loop_depth -= 1;
                    return Err(e);
                }
            }
        }
        self.loop_depth -= 1;
        propagate.map_or(Ok(status), |sig| Err(anyhow::Error::new(sig)))
    }

    pub(super) fn run_case(&mut self, cc: &CaseClause) -> Result<ExitStatus> {
        // Neither field-split nor pathname-expanded.
        let word = self.expand_word_to_string(&cc.word)?;
        let mut status = ExitStatus::SUCCESS;
        let mut fall_through = false;
        for arm in &cc.arms {
            let matched = fall_through || self.arm_matches(arm, &word)?;
            fall_through = false;
            if !matched {
                continue;
            }
            status = self.run_list(&arm.body)?;
            match arm.end {
                CaseEnd::Break => return Ok(status),
                CaseEnd::FallThrough => fall_through = true,
                CaseEnd::Continue => {}
            }
        }
        Ok(status)
    }

    fn arm_matches(&mut self, arm: &CaseArm, word: &str) -> Result<bool> {
        for pattern in &arm.patterns {
            let nocase = self.shopt.nocasematch;
            if glob_match_case(&self.expand_pattern(pattern)?, word, nocase) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(super) fn run_group(&mut self, gc: &GroupCmd) -> Result<ExitStatus> {
        if gc.subshell {
            // SAFETY: fork.
            match unsafe { fork()? } {
                Fork::Child => {
                    // SAFETY: in child, before any allocations.
                    unsafe { restore_child_signals(self.interactive || self.monitor) };
                    self.reset_traps_for_subshell();
                    let status = match self.run_list(&gc.body) {
                        Ok(s) => s,
                        Err(e) => {
                            if !is_break(&e) && !is_continue(&e) && !is_return(&e) {
                                emit(e);
                            }
                            ExitStatus::FAILURE
                        }
                    };
                    self.exit(status.0);
                }
                Fork::ParentOf(child) => return self.wait_for_pid(child),
            }
        }
        self.run_list(&gc.body)
    }
}
