use crate::sys::{Fork, fork};
use anyhow::Result;
use rustix::io::Errno;
use rustix::process::{WaitOptions, waitpid};

use crate::ast::{Command, Word};
use crate::errfmt::emit;
use crate::expand::{
    ParamOp, eval_arith, expand_tilde, glob_expand, parse_param_op, strip_prefix, strip_suffix,
};
use crate::fd::{close_raw, dup2_raw, raw_pipe, read_raw};
use crate::jobs::ExitStatus;
use crate::parser::parse;
use crate::signal::restore_child_signals;

use super::{Shell, is_assignment_word, is_break, is_continue, is_return};

impl Shell {
    /// # Errors
    ///
    /// Returns an error if expanding `word` fails (e.g. a command
    /// substitution's command fails to run, or an unset variable is
    /// referenced under `set -u`).
    pub fn expand_word(&mut self, word: &Word) -> Result<Vec<String>> {
        if let Word::Quoted(inner) = word {
            return Ok(vec![self.expand_word_to_string_inner(inner, true)?]);
        }
        let raw = self.expand_word_to_string(word)?;
        // Only '*'/'?' trigger a directory scan: `glob_match` (src/expand.rs)
        // has no bracket-expression support, so a `[` here never changes
        // what actually matches. Treating a bare `[` as a possible glob was
        // pure overhead: the ubiquitous `[` test command is itself exactly
        // one such word, so every `[ ... ]` invocation paid for a full
        // `read_dir` (open+getdents64+close) with no matching payoff.
        if raw.contains('*') || raw.contains('?') {
            let matches = glob_expand(&raw);
            if !matches.is_empty() {
                return Ok(matches);
            }
        }
        Ok(vec![raw])
    }

    /// Expands `words` into fields: unquoted expansion results are split
    /// on `$IFS` and pathname-expanded, `"$@"` yields one field per
    /// positional parameter, and an unquoted word that expands to nothing
    /// yields no field at all.
    ///
    /// # Errors
    ///
    /// Returns an error if expanding any word fails.
    pub fn expand_words(&mut self, words: &[Word]) -> Result<Vec<String>> {
        let ifs = self.env.get("IFS").unwrap_or_else(|| " \t\n".to_owned());
        let mut fields = Fields::new(&ifs, words.len());
        for word in words {
            self.expand_into_fields(word, false, &mut fields)?;
            fields.finish(false);
        }
        Ok(fields.out)
    }

    /// [`Self::expand_words`] for a command's words, except that the
    /// `NAME=value` operands of `export`, `readonly` and `local` keep their
    /// value whole, as they would in a plain assignment.
    pub(super) fn expand_command_words(&mut self, words: &[Word]) -> Result<Vec<String>> {
        let declaration = matches!(
            words.first(),
            Some(Word::Literal(name)) if matches!(name.as_str(), "export" | "readonly" | "local")
        );
        if !declaration {
            return self.expand_words(words);
        }
        let mut out = Vec::with_capacity(words.len());
        for word in words {
            if is_assignment_word(word) {
                out.push(self.expand_assignment(word)?);
            } else {
                out.extend(self.expand_words(std::slice::from_ref(word))?);
            }
        }
        Ok(out)
    }

    /// Expands a `NAME=value` word as one string, without field splitting
    /// or pathname expansion. A `~` at the start of the value, or after a
    /// `:` in its leading literal text, is tilde-expanded.
    pub(super) fn expand_assignment(&mut self, word: &Word) -> Result<String> {
        let (head, rest) = match word {
            Word::Literal(s) => (s.as_str(), &[][..]),
            Word::Compound(parts) => match parts.split_first() {
                Some((Word::Literal(s), rest)) => (s.as_str(), rest),
                _ => return self.expand_word_to_string(word),
            },
            _ => return self.expand_word_to_string(word),
        };
        let Some((name, value)) = head.split_once('=') else {
            return self.expand_word_to_string(word);
        };
        let mut out = format!("{name}=");
        for (i, segment) in value.split(':').enumerate() {
            if i > 0 {
                out.push(':');
            }
            out.push_str(&expand_tilde(segment, &self.env));
        }
        for part in rest {
            out.push_str(&self.expand_word_to_string(part)?);
        }
        Ok(out)
    }

    fn expand_into_fields(
        &mut self,
        word: &Word,
        in_quotes: bool,
        fields: &mut Fields<'_>,
    ) -> Result<()> {
        match word {
            Word::Literal(s) if in_quotes => fields.push_quoted(s),
            Word::Literal(s) if s.starts_with('~') => {
                fields.push_unquoted(&expand_tilde(s, &self.env));
            }
            Word::Literal(s) => fields.push_unquoted(s),
            Word::Var(name) if name == "@" || name == "*" => {
                let params = self.env.positional_args().to_vec();
                if in_quotes && name == "*" {
                    let sep = fields
                        .ifs
                        .chars()
                        .next()
                        .map(String::from)
                        .unwrap_or_default();
                    fields.push_quoted(&params.join(&sep));
                } else {
                    for (i, param) in params.iter().enumerate() {
                        if i > 0 {
                            fields.finish(in_quotes);
                        }
                        if in_quotes {
                            fields.push_quoted(param);
                        } else {
                            fields.push_split(param);
                        }
                    }
                }
            }
            Word::Var(_) | Word::CmdSub(_) | Word::Arith(_) => {
                let value = self.expand_word_to_string_inner(word, in_quotes)?;
                if in_quotes {
                    fields.push_quoted(&value);
                } else {
                    fields.push_split(&value);
                }
            }
            Word::Compound(parts) => {
                for part in parts {
                    self.expand_into_fields(part, in_quotes, fields)?;
                }
            }
            Word::Quoted(inner) => {
                // `"$@"` with no positional parameters must vanish entirely,
                // so only quoting without it forces the field to exist.
                if !contains_at(inner) {
                    fields.quoted = true;
                }
                self.expand_into_fields(inner, true, fields)?;
            }
        }
        Ok(())
    }

    /// # Errors
    ///
    /// Returns an error if expanding `word` fails.
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
            Word::Var(name) => self.expand_var(name),
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
    /// hard error (`Illegal number: <value>`) rather than a silent `0` or
    /// being recursively re-interpreted as yet another variable name.
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
                    let value = self.expand_var(&var)?;
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

    /// # Errors
    ///
    /// Returns an error if `name` is unset under `set -u`, or if a nested
    /// command substitution fails.
    pub fn expand_var(&mut self, name: &str) -> Result<String> {
        match name {
            "?" | "$" | "0" | "@" | "*" | "#" => self.resolve_param_checked(name),
            n if n.chars().all(|c| c.is_ascii_digit()) => self.resolve_param_checked(n),
            name => match parse_param_op(name) {
                Some(ParamOp::Length(var)) => {
                    Ok(self.resolve_param_checked(var)?.len().to_string())
                }
                Some(ParamOp::PrefixStrip { var, pat, greedy }) => {
                    let val = self.resolve_param_checked(var)?;
                    Ok(strip_prefix(&val, pat, greedy))
                }
                Some(ParamOp::SuffixStrip { var, pat, greedy }) => {
                    let val = self.resolve_param_checked(var)?;
                    Ok(strip_suffix(&val, pat, greedy))
                }
                Some(ParamOp::Conditional { var, op, word }) => {
                    // Unchecked (`resolve_param`, not `_checked`): `:-`,
                    // `:=`, `:+`, and `:?` all explicitly handle `var`
                    // being unset themselves, so they're exempt from
                    // `nounset` on `var` specifically, even though every
                    // other reference to it is not.
                    let val = self.resolve_param(var);
                    // `word` is raw, unexpanded source text (`parse_param_op`
                    // only slices it out, it never parses it); a `$ref`
                    // inside it needs the same substitution any other word
                    // gets, e.g. `${X:-$Y}` must expand `$Y`, not use it
                    // literally.
                    match op {
                        ":-" => {
                            if val.is_empty() {
                                self.expand_raw_text(word)
                            } else {
                                Ok(val)
                            }
                        }
                        ":+" => {
                            if val.is_empty() {
                                Ok(String::new())
                            } else {
                                self.expand_raw_text(word)
                            }
                        }
                        ":?" => {
                            if val.is_empty() {
                                let msg = if word.is_empty() {
                                    "parameter not set".to_owned()
                                } else {
                                    self.expand_raw_text(word)?
                                };
                                emit(format!("{var}: {msg}"));
                                if !self.interactive {
                                    std::process::exit(1);
                                }
                            }
                            Ok(val)
                        }
                        ":=" => {
                            if val.is_empty() {
                                let expanded = self.expand_raw_text(word)?;
                                self.env.set(var, &expanded);
                                Ok(expanded)
                            } else {
                                Ok(val)
                            }
                        }
                        _ => Ok(val),
                    }
                }
                None => self.resolve_param_checked(name),
            },
        }
    }

    /// Resolves a bare parameter name to its value: the special parameters
    /// (`$?`, `$$`, `$0`, `$1`.., `$@`/`$*`, `$#`) plus a plain env lookup
    /// for everything else. `expand_var`'s operator branches (`${#var}`,
    /// `${var#pat}`, `${var:-word}`, ...) route the name they parsed out
    /// through here too, rather than reading `self.env` directly, so
    /// `${1:-x}`, `${#@}`, `${?:-x}`, etc. resolve the same special
    /// parameter a bare `$1`/`$#`/`$?` would instead of always seeing
    /// "unset" (positional args and the other special parameters aren't
    /// stored in `self.env`, so a raw lookup by that literal name never
    /// finds them).
    ///
    /// Never fails: `""` for anything unset, regardless of `nounset`. Only
    /// `resolve_param_checked` (below) enforces that; this raw accessor is
    /// what the `:-`/`:=`/`:+`/`:?` operators call directly, since a
    /// variable they're already handling the unset-case of is by
    /// definition exempt from `nounset` on that reference.
    fn resolve_param(&self, name: &str) -> String {
        match name {
            "?" => self.last_status.0.to_string(),
            "!" => self
                .last_bg_pid
                .map(|p| p.as_raw_nonzero().to_string())
                .unwrap_or_default(),
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
            name => self.env.get(name).unwrap_or_default(),
        }
    }

    /// Whether `name` counts as "set" for `nounset` purposes: the special
    /// parameters are always set (even `$!` before any background job has
    /// run), a positional parameter is set only within `$#`'s current
    /// range (`$5` with only 3 positional params given *does* trigger
    /// `nounset`), and anything else is set iff `Env` actually has it.
    fn is_param_set(&self, name: &str) -> bool {
        match name {
            "?" | "!" | "$" | "0" | "@" | "*" | "#" => true,
            n if n.chars().all(|c| c.is_ascii_digit()) => {
                let idx: usize = n.parse().unwrap_or(0);
                idx >= 1 && idx <= self.env.positional_args().len()
            }
            name => self.env.get(name).is_some(),
        }
    }

    /// Like `resolve_param`, but honors `-u`/`nounset`: errors instead of
    /// silently returning `""` if `name` is genuinely unset. This is the
    /// path every *direct* parameter reference goes through (bare `$var`,
    /// `${var}`, `${#var}`, `${var#pat}`/`${var%pat}`); the one exception
    /// (`:-`/`:=`/`:+`/`:?`'s own tested variable) calls `resolve_param`
    /// directly instead, see its call site above.
    ///
    /// A non-interactive shell exits outright on this, the same "this
    /// isn't a normal command failure, abort the script" treatment a real
    /// syntax error gets; an interactive shell just fails the current
    /// command and returns to the prompt, so that case is a normal
    /// `bail!` for the usual catch-and-continue path to handle. The exit
    /// code itself is 127 normally, but 1 if `errexit` is *also* set:
    /// `set -u` alone exits 127 on an unbound variable, `set -eu` exits 1
    /// for the identical failure.
    fn resolve_param_checked(&self, name: &str) -> Result<String> {
        if self.nounset && !self.is_param_set(name) {
            if self.interactive {
                anyhow::bail!("{name}: unbound variable");
            }
            emit(format!("{name}: unbound variable"));
            std::process::exit(if self.errexit { 1 } else { 127 });
        }
        Ok(self.resolve_param(name))
    }

    fn expand_cmd_sub(&self, cmd: &Command) -> Result<String> {
        let (read_fd, write_fd) = raw_pipe()?;
        // SAFETY: fork.
        match unsafe { fork()? } {
            Fork::Child => {
                // SAFETY: in child, before any allocations.
                unsafe { restore_child_signals(self.interactive) };
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

    /// # Errors
    ///
    /// Returns an error if expanding `body`'s embedded references fails.
    pub fn expand_heredoc_body(&mut self, body: &str) -> Result<String> {
        let mut result = self.expand_raw_text(body)?;
        if !result.ends_with('\n') {
            result.push('\n');
        }
        Ok(result)
    }

    /// Expands `$var`/`${...}`/`$(...)`/`$((...))` references in already-materialized
    /// text, as opposed to the `Word` AST nodes the parser produces:
    /// wherever raw text picked up after parsing needs the same
    /// substitutions applied by hand. Used for here-doc bodies and for the
    /// default/alternate word in `${var:-word}`-style parameter-expansion
    /// operators (`parse_param_op` hands that word back as an unexpanded
    /// slice of the original source, since it's only extracted, never
    /// parsed into `Word`s), and for the value of `$ENV` at startup.
    /// As inside double quotes, a backslash escapes only `$`, `` ` ``, `\`
    /// and newline; before anything else it stays literal.
    ///
    /// # Errors
    ///
    /// Returns an error if expanding `text`'s embedded references fails.
    pub fn expand_raw_text(&mut self, text: &str) -> Result<String> {
        let mut result = String::with_capacity(text.len());
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\\' {
                match chars.next_if(|n| matches!(n, '$' | '`' | '\\' | '\n')) {
                    Some('\n') => {}
                    Some(escaped) => result.push(escaped),
                    None => result.push('\\'),
                }
                continue;
            }
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
                    result.push_str(&self.expand_var(&var)?);
                }
                Some('(') => {
                    chars.next();
                    let arith = chars.next_if_eq(&'(').is_some();
                    let mut depth = if arith { 2usize } else { 1 };
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
                    if arith {
                        if cmd_src.ends_with(')') {
                            cmd_src.pop();
                        }
                        let expanded = self.expand_arith_vars(&cmd_src)?;
                        result.push_str(&eval_arith(&expanded).to_string());
                    } else if let Ok(program) = parse(&cmd_src)
                        && let Ok(s) = self.expand_cmd_sub(&program.into_command())
                    {
                        result.push_str(&s);
                    }
                }
                Some(c2) if c2.is_ascii_alphanumeric() || c2 == '_' || "@*#?-$!".contains(c2) => {
                    if "@*#?-$!".contains(c2) {
                        chars.next();
                        result.push_str(&self.expand_var(&c2.to_string())?);
                    } else {
                        var.push_str(&take_identifier(&mut chars));
                        result.push_str(&self.expand_var(&var)?);
                    }
                }
                _ => result.push('$'),
            }
        }
        Ok(result)
    }
}

/// Fields built up while expanding a list of words (POSIX 2.6.5, 2.6.6).
struct Fields<'a> {
    ifs: &'a str,
    out: Vec<String>,
    cur: String,
    /// The current field contains a quoted part, so it is kept even if empty.
    quoted: bool,
    /// The current field contains an unquoted `*` or `?`.
    glob: bool,
}

impl<'a> Fields<'a> {
    fn new(ifs: &'a str, capacity: usize) -> Self {
        Self {
            ifs,
            out: Vec::with_capacity(capacity),
            cur: String::new(),
            quoted: false,
            glob: false,
        }
    }

    fn push_quoted(&mut self, s: &str) {
        self.cur.push_str(s);
        self.quoted = true;
    }

    fn push_unquoted(&mut self, s: &str) {
        self.glob |= s.contains(['*', '?']);
        self.cur.push_str(s);
    }

    /// Appends an unquoted expansion result, ending a field at each `$IFS`
    /// delimiter: a run of IFS whitespace, optionally around one other IFS
    /// character. Leading and trailing IFS whitespace only ends the field
    /// before it; a non-whitespace delimiter always produces one.
    fn push_split(&mut self, value: &str) {
        let ifs = self.ifs;
        if !value.contains(|c| ifs.contains(c)) {
            self.push_unquoted(value);
            return;
        }
        let is_ws = |c: char| matches!(c, ' ' | '\t' | '\n') && ifs.contains(c);
        let is_delim = |c: char| !matches!(c, ' ' | '\t' | '\n') && ifs.contains(c);
        let mut chars = value.chars().peekable();
        if chars.next_if(|&c| is_ws(c)).is_some() {
            while chars.next_if(|&c| is_ws(c)).is_some() {}
            self.finish(false);
        }
        while let Some(c) = chars.next() {
            if !is_ws(c) && !is_delim(c) {
                self.glob |= matches!(c, '*' | '?');
                self.cur.push(c);
                continue;
            }
            let mut hard = is_delim(c);
            while let Some(n) = chars.next_if(|&n| is_ws(n) || (!hard && is_delim(n))) {
                hard |= is_delim(n);
            }
            self.finish(hard || chars.peek().is_some());
        }
    }

    /// Ends the current field, keeping it if `force`d, quoted, or non-empty.
    fn finish(&mut self, force: bool) {
        if force || self.quoted || !self.cur.is_empty() {
            let field = std::mem::take(&mut self.cur);
            let matches = if self.glob {
                glob_expand(&field)
            } else {
                Vec::new()
            };
            if matches.is_empty() {
                self.out.push(field);
            } else {
                self.out.extend(matches);
            }
        }
        self.quoted = false;
        self.glob = false;
    }
}

fn contains_at(word: &Word) -> bool {
    match word {
        Word::Var(name) => name == "@",
        Word::Compound(parts) => parts.iter().any(contains_at),
        Word::Quoted(inner) => contains_at(inner),
        _ => false,
    }
}

/// Consumes a run of identifier characters (`[A-Za-z0-9_]`) from `chars`.
/// Used wherever a bare variable name needs to be read out of already-
/// expanded text (arithmetic expressions, here-doc bodies): the lexer
/// handles this for source text, but these two run over materialized
/// strings at execution time, after lexing is long done.
fn take_identifier(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
    let mut ident = String::new();
    while let Some(c) = chars.next_if(|c| c.is_ascii_alphanumeric() || *c == '_') {
        ident.push(c);
    }
    ident
}
