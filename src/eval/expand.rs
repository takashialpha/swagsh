use anyhow::Result;
use rustix::io::Errno;
use rustix::process::{WaitOptions, waitpid};
use rustix::runtime::{Fork, kernel_fork};

use crate::ast::{Command, Word};
use crate::errfmt::emit;
use crate::expand::{
    ParamOp, eval_arith, expand_tilde, glob_expand, parse_param_op, strip_prefix, strip_suffix,
};
use crate::fd::{close_raw, dup2_raw, raw_pipe, read_raw};
use crate::jobs::ExitStatus;
use crate::parser::parse;
use crate::signal::restore_child_signals;

use super::{Shell, is_break, is_continue, is_return};

impl Shell {
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
                let expanded = self.expand_arith_vars(expr)?;
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

    /// Substitutes `$var`/`${var}` *and* bare `var` references inside a
    /// `$(( ))` expression with their values, before handing the result to
    /// `eval_arith`. POSIX arithmetic treats a bare identifier the same as
    /// `$identifier`: `$(( x + 1 ))` means the same thing as
    /// `$(( $x + 1 ))`, so both forms are recognized here; `eval_arith`'s
    /// tokenizer has no variable lookup of its own; it treats any
    /// identifier that reaches it as `0`, which is only correct once every
    /// reference has already been substituted.
    ///
    /// Delegates each reference to `expand_var` (the same variable expander
    /// `Word::Var` uses for ordinary word expansion) rather than reading
    /// `self.env` directly, so parameter-expansion operators work inside
    /// arithmetic too: `$(( ${n:-0} + 1 ))` needs the `:-` default applied,
    /// not just a raw lookup of a variable literally named `n:-0`. Follows
    /// dash's rule for what a substitution is allowed to be: unset (empty)
    /// becomes `0` silently, but a variable holding non-numeric text is a
    /// hard error (`Illegal number: <value>`) rather than a silent `0`:
    /// dash does not recursively treat that text as another variable name
    /// (unlike bash), it just rejects it.
    fn expand_arith_vars(&mut self, expr: &str) -> Result<String> {
        let mut result = String::new();
        let mut chars = expr.chars().peekable();
        while let Some(c) = chars.next() {
            let var = match c {
                '$' => Some(match chars.peek() {
                    Some(&'{') => {
                        chars.next();
                        let mut var = String::new();
                        for ch in chars.by_ref() {
                            if ch == '}' {
                                break;
                            }
                            var.push(ch);
                        }
                        var
                    }
                    Some(&c2) if "@*#?-$!".contains(c2) => {
                        chars.next();
                        c2.to_string()
                    }
                    _ => take_identifier(&mut chars),
                }),
                c if c.is_ascii_alphabetic() || c == '_' => {
                    let mut var = String::from(c);
                    var.push_str(&take_identifier(&mut chars));
                    Some(var)
                }
                _ => None,
            };

            match var {
                Some(var) => {
                    let value = self.expand_var(&var);
                    let trimmed = value.trim();
                    if trimmed.is_empty() {
                        result.push('0');
                    } else if trimmed.parse::<i64>().is_ok() {
                        result.push_str(trimmed);
                    } else {
                        anyhow::bail!("Illegal number: {trimmed}");
                    }
                }
                None => result.push(c),
            }
        }
        Ok(result)
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
                // SAFETY: in child, before any allocations.
                unsafe { restore_child_signals() };
                close_raw(read_fd);
                let _ = dup2_raw(write_fd, 1);
                close_raw(write_fd);
                let mut child = Self::new(self.env.clone(), false);
                let status = match child.run_command(cmd) {
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
            Fork::ParentOf(child) => {
                close_raw(write_fd);
                let mut output = Vec::new();
                let mut buf = [0u8; 512];
                loop {
                    match read_raw(read_fd, &mut buf) {
                        Ok(0) => break,
                        Ok(n) => output.extend_from_slice(&buf[..n]),
                        Err(e) if e == Errno::INTR => {}
                        Err(e) => return Err(e.into()),
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

    pub fn expand_heredoc_body(&mut self, body: &str) -> String {
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
                        var.push_str(&take_identifier(&mut chars));
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

/// Consumes a run of identifier characters (`[A-Za-z0-9_]`) from `chars`.
/// Used wherever a bare variable name needs to be read out of already-
/// expanded text (arithmetic expressions, here-doc bodies): the lexer
/// handles this for source text, but these two run over materialized
/// strings at execution time, after lexing is long done.
fn take_identifier(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
    let mut ident = String::new();
    while matches!(chars.peek(), Some(c) if c.is_ascii_alphanumeric() || *c == '_') {
        ident.push(chars.next().unwrap());
    }
    ident
}
