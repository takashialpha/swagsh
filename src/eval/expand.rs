use anyhow::{Result, anyhow};
use rustix::io::Errno;
use rustix::process::{WaitOptions, waitpid};
use rustix::runtime::{Fork, kernel_fork};

use crate::ast::{Command, Word};
use crate::expand::{
    ParamOp, eval_arith, expand_tilde, glob_expand, parse_param_op, strip_prefix, strip_suffix,
};
use crate::fd::{close_raw, dup2_raw, raw_pipe, read_raw};
use crate::jobs::ExitStatus;
use crate::parser::parse;

use super::Shell;

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
