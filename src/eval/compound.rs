use anyhow::Result;
use rustix::runtime::{Fork, kernel_fork};

use crate::ast::{AndOrList, AndOrOp, CaseClause, ForClause, GroupCmd, IfClause, WhileClause};
use crate::errfmt::emit;
use crate::expand::glob_match;
use crate::jobs::ExitStatus;
use crate::signal::restore_child_signals;

use super::{Shell, is_break, is_continue, is_return};

impl Shell {
    pub(super) fn run_and_or(&mut self, aol: &AndOrList) -> Result<ExitStatus> {
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

    pub fn run_list(&mut self, list: &[AndOrList]) -> Result<ExitStatus> {
        let mut status = ExitStatus::SUCCESS;
        for aol in list {
            status = self.run_and_or(aol)?;
        }
        Ok(status)
    }

    pub(super) fn run_if(&mut self, ic: &IfClause) -> Result<ExitStatus> {
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

    pub(super) fn run_for(&mut self, fc: &ForClause) -> Result<ExitStatus> {
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

    pub(super) fn run_while(&mut self, wc: &WhileClause) -> Result<ExitStatus> {
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

    pub(super) fn run_case(&mut self, cc: &CaseClause) -> Result<ExitStatus> {
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

    pub(super) fn run_group(&mut self, gc: &GroupCmd) -> Result<ExitStatus> {
        if gc.subshell {
            // SAFETY: fork.
            match unsafe { kernel_fork()? } {
                Fork::Child(_) => {
                    // SAFETY: in child, before any allocations.
                    unsafe { restore_child_signals() };
                    let status = match self.run_list(&gc.body) {
                        Ok(s) => s,
                        Err(e) => {
                            if !is_break(&e) && !is_continue(&e) && !is_return(&e) {
                                emit(e);
                            }
                            ExitStatus::FAILURE
                        }
                    };
                    std::process::exit(status.0);
                }
                Fork::ParentOf(child) => return self.wait_for_pid(child),
            }
        }
        self.run_list(&gc.body)
    }
}
