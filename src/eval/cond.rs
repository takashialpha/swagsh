//! `[[ expression ]]`.

use anyhow::{Result, anyhow};

use crate::ast::{CondExpr, Word};
use crate::builtins::test::{file_compare, integer_operator, unary_test};
use crate::env::{Array, Key};
use crate::expand::{eval_arith_with, glob_match_case};
use crate::jobs::ExitStatus;

use super::Shell;

impl Shell {
    /// `[[ expression ]]`: status 0 when it holds, 1 when not, 2 when it
    /// cannot be evaluated (a bad regular expression, say).
    pub(super) fn run_cond(&mut self, expr: &CondExpr) -> Result<ExitStatus> {
        if self.xtrace {
            let ps4 = self.xtrace_prefix();
            eprintln!("{ps4}[[ ... ]]");
        }
        match self.eval_cond(expr) {
            Ok(true) => Ok(ExitStatus::SUCCESS),
            Ok(false) => Ok(ExitStatus::FAILURE),
            Err(e) if e.is::<CondError>() => {
                crate::errfmt::emit(&e);
                Ok(ExitStatus(2))
            }
            Err(e) => Err(e),
        }
    }

    fn eval_cond(&mut self, expr: &CondExpr) -> Result<bool> {
        Ok(match expr {
            CondExpr::Word(w) => !self.cond_word(w)?.is_empty(),
            CondExpr::Not(inner) => !self.eval_cond(inner)?,
            CondExpr::And(a, b) => self.eval_cond(a)? && self.eval_cond(b)?,
            CondExpr::Or(a, b) => self.eval_cond(a)? || self.eval_cond(b)?,
            CondExpr::Unary(op, w) => {
                let operand = self.cond_word(w)?;
                unary_test(self, op, &operand).unwrap_or(false)
            }
            CondExpr::Binary(a, op, b) => self.eval_binary(a, op, b)?,
        })
    }

    fn eval_binary(&mut self, a: &Word, op: &str, b: &Word) -> Result<bool> {
        let lhs = self.cond_word(a)?;
        Ok(match op {
            "==" | "=" => glob_match_case(&self.expand_pattern(b)?, &lhs, self.shopt.nocasematch),
            "!=" => !glob_match_case(&self.expand_pattern(b)?, &lhs, self.shopt.nocasematch),
            "=~" => {
                let pattern = self.expand_regex(b, false)?;
                self.regex_match(&lhs, &pattern)?
            }
            "<" | ">" => {
                let rhs = self.cond_word(b)?;
                let order = crate::collate::compare(&lhs, &rhs);
                order
                    == if op == "<" {
                        std::cmp::Ordering::Less
                    } else {
                        std::cmp::Ordering::Greater
                    }
            }
            _ if let Some(cmp) = integer_operator(op) => {
                let rhs = self.cond_word(b)?;
                let x = eval_arith_with(&lhs, &mut self.env)?;
                let y = eval_arith_with(&rhs, &mut self.env)?;
                cmp(x, y)
            }
            _ => {
                let rhs = self.cond_word(b)?;
                file_compare(&lhs, op, &rhs).unwrap_or(false)
            }
        })
    }

    /// An operand, expanded without field splitting or pathname expansion.
    fn cond_word(&mut self, w: &Word) -> Result<String> {
        self.expand_word_to_string(w)
    }

    /// The right side of `=~` as a regular expression: quoted parts match
    /// themselves.
    fn expand_regex(&mut self, word: &Word, in_quotes: bool) -> Result<String> {
        match word {
            Word::Compound(parts) => {
                let mut out = String::new();
                for part in parts {
                    out.push_str(&self.expand_regex(part, in_quotes)?);
                }
                Ok(out)
            }
            Word::Quoted(inner) => self.expand_regex(inner, true),
            _ => {
                let s = self.expand_word_to_string_inner(word, in_quotes)?;
                Ok(if in_quotes { escape_regex(&s) } else { s })
            }
        }
    }

    /// Matches `text` against `pattern`, setting `BASH_REMATCH` to the
    /// match and its groups (empty when nothing matched).
    fn regex_match(&mut self, text: &str, pattern: &str) -> Result<bool> {
        let found = crate::sys::regex_match(pattern, text, self.shopt.nocasematch)
            .map_err(|msg| anyhow!(CondError(format!("{pattern}: {msg}"))))?;
        let mut array = Array::new(false);
        let matched = found.is_some();
        for (i, group) in found.into_iter().flatten().enumerate() {
            let value = group
                .and_then(|(start, end)| text.get(start..end))
                .unwrap_or_default();
            array.set(Key::Index(i), value.to_owned());
        }
        if !self.env.is_readonly("BASH_REMATCH") {
            self.env.set_array("BASH_REMATCH", array)?;
        }
        Ok(matched)
    }
}

/// `s` with the characters special in an extended regular expression
/// escaped.
fn escape_regex(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "\\^$.|?*+()[]{}".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// A `[[ ... ]]` expression that cannot be evaluated, which gives status 2.
#[derive(Debug)]
struct CondError(String);

impl std::fmt::Display for CondError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CondError {}
