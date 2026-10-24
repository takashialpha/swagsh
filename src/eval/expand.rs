use crate::sys::{Fork, fork};
use anyhow::{Result, anyhow};
use rustix::io::Errno;
use rustix::process::{WaitOptions, waitpid};

use crate::ast::{Command, Word};
use crate::errfmt::emit;
use crate::expand::{
    ParamOp, escape_glob, eval_arith_with, expand_tilde, glob_expand, has_glob_syntax,
    parse_param_op, strip_prefix, strip_suffix,
};
use crate::fd::{close_raw, dup2_raw, raw_pipe, read_raw};
use crate::jobs::ExitStatus;
use crate::lexer::{QUOTE_END, QUOTE_START};
use crate::parser::{parse, parse_word};
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
        if !self.noglob && has_glob_syntax(&raw) {
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
        fields.noglob = self.noglob;
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
        let mut out = String::with_capacity(head.len());
        if value.contains('~') {
            out.push_str(name);
            out.push('=');
            for (i, segment) in value.split(':').enumerate() {
                if i > 0 {
                    out.push(':');
                }
                out.push_str(&expand_tilde(segment, &self.env));
            }
        } else {
            out.push_str(head);
        }
        for part in rest {
            out.push_str(&self.expand_word_to_string(part)?);
        }
        Ok(out)
    }

    /// Expands `word` into a pattern for [`glob_match`](crate::expand::glob_match): quoted parts are
    /// escaped so they match literally, unquoted ones keep their meaning.
    pub(super) fn expand_pattern(&mut self, word: &Word) -> Result<String> {
        match word {
            Word::Literal(s) if s.starts_with('~') => Ok(escape_glob(&expand_tilde(s, &self.env))),
            Word::Compound(parts) => {
                let mut out = String::new();
                for (i, part) in parts.iter().enumerate() {
                    match part {
                        Word::Literal(s) if i == 0 && s.starts_with('~') => {
                            let expanded = self.tilde_prefix(s, false);
                            out.push_str(&if expanded == *s {
                                expanded
                            } else {
                                escape_glob(&expanded)
                            });
                        }
                        _ => out.push_str(&self.expand_pattern_inner(part, false)?),
                    }
                }
                Ok(out)
            }
            _ => self.expand_pattern_inner(word, false),
        }
    }

    /// Tilde-expands the literal `s` that begins a word. The tilde prefix
    /// runs to the first `/`, so when `s` has none and more of the word
    /// follows (`~"x"`, `~$u`), part of the prefix is quoted or expanded and
    /// POSIX leaves it alone.
    fn tilde_prefix(&self, s: &str, whole_word: bool) -> String {
        if whole_word || s.contains('/') {
            expand_tilde(s, &self.env).into_owned()
        } else {
            s.to_owned()
        }
    }

    fn expand_pattern_inner(&mut self, word: &Word, in_quotes: bool) -> Result<String> {
        match word {
            Word::Compound(parts) => {
                let mut out = String::new();
                for part in parts {
                    out.push_str(&self.expand_pattern_inner(part, in_quotes)?);
                }
                Ok(out)
            }
            Word::Quoted(inner) => self.expand_pattern_inner(inner, true),
            _ => {
                let s = self.expand_word_to_string_inner(word, in_quotes)?;
                Ok(if in_quotes { escape_glob(&s) } else { s })
            }
        }
    }

    fn push_positional(&self, star: bool, in_quotes: bool, fields: &mut Fields<'_>) {
        let params = self.env.positional_args();
        if in_quotes && star {
            let sep = fields
                .ifs
                .chars()
                .next()
                .map(String::from)
                .unwrap_or_default();
            fields.push_quoted(&params.join(&sep));
            return;
        }
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

    /// Like [`Self::expand_into_fields`] for the operand of `${var:-word}`,
    /// whose unquoted literal text is field-split like an expansion result.
    fn expand_operand_into_fields(
        &mut self,
        word: &Word,
        in_quotes: bool,
        fields: &mut Fields<'_>,
    ) -> Result<()> {
        match word {
            Word::Literal(s) if !in_quotes && fields.at_word_start() => {
                fields.push_split(&expand_tilde(s, &self.env));
                Ok(())
            }
            Word::Literal(s) if !in_quotes => {
                fields.push_split(s);
                Ok(())
            }
            Word::Compound(parts) => {
                for (i, part) in parts.iter().enumerate() {
                    match part {
                        Word::Literal(s) if i == 0 && !in_quotes && fields.at_word_start() => {
                            fields.push_split(&self.tilde_prefix(s, parts.len() == 1));
                        }
                        _ => self.expand_operand_into_fields(part, in_quotes, fields)?,
                    }
                }
                Ok(())
            }
            _ => self.expand_into_fields(word, in_quotes, fields),
        }
    }

    fn expand_into_fields(
        &mut self,
        word: &Word,
        in_quotes: bool,
        fields: &mut Fields<'_>,
    ) -> Result<()> {
        match word {
            Word::Literal(s) if in_quotes => fields.push_quoted(s),
            Word::Literal(s) if fields.at_word_start() => {
                fields.push_unquoted(&expand_tilde(s, &self.env));
            }
            Word::Literal(s) => fields.push_unquoted(s),
            Word::Var(name) if name == "@" || name == "*" => {
                self.push_positional(name == "*", in_quotes, fields);
            }
            Word::Var(name) => {
                if let Some(ParamOp::Conditional { var, op, word }) =
                    has_param_op(name).then(|| parse_param_op(name)).flatten()
                {
                    match self.resolve_conditional(var, op, word, in_quotes)? {
                        Conditional::Value(_) if var == "@" || var == "*" => {
                            self.push_positional(var == "*", in_quotes, fields);
                        }
                        Conditional::Value(val) if in_quotes => fields.push_quoted(&val),
                        Conditional::Value(val) => fields.push_split(&val),
                        Conditional::Word(raw) if is_plain_operand(raw) => {
                            if in_quotes {
                                fields.push_quoted(raw);
                            } else {
                                fields.push_split(raw);
                            }
                        }
                        Conditional::Word(raw) => {
                            let word = parse_word(raw, in_quotes, Some(&self.env.aliases()))?;
                            self.expand_operand_into_fields(&word, in_quotes, fields)?;
                        }
                    }
                } else {
                    let value = self.expand_var_in(name, in_quotes)?;
                    if in_quotes {
                        fields.push_quoted(&value);
                    } else {
                        fields.push_split(&value);
                    }
                }
            }
            Word::CmdSub(_) | Word::Arith(_) => {
                let value = self.expand_word_to_string_inner(word, in_quotes)?;
                if in_quotes {
                    fields.push_quoted(&value);
                } else {
                    fields.push_split(&value);
                }
            }
            Word::Compound(parts) => {
                for (i, part) in parts.iter().enumerate() {
                    match part {
                        Word::Literal(s) if i == 0 && !in_quotes && fields.at_word_start() => {
                            if s.starts_with('~') {
                                fields.push_unquoted(&self.tilde_prefix(s, parts.len() == 1));
                            } else {
                                fields.push_unquoted(s);
                            }
                        }
                        _ => self.expand_into_fields(part, in_quotes, fields)?,
                    }
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
        match word {
            Word::Literal(s) => Ok(expand_tilde(s, &self.env).into_owned()),
            Word::Compound(parts) => {
                let mut result = String::new();
                for (i, part) in parts.iter().enumerate() {
                    match part {
                        Word::Literal(s) if i == 0 => {
                            result.push_str(&self.tilde_prefix(s, parts.len() == 1));
                        }
                        _ => result.push_str(&self.expand_word_to_string_inner(part, false)?),
                    }
                }
                Ok(result)
            }
            _ => self.expand_word_to_string_inner(word, false),
        }
    }

    fn expand_word_to_string_inner(&mut self, word: &Word, in_quotes: bool) -> Result<String> {
        match word {
            Word::Literal(s) => Ok(s.clone()),
            Word::Var(name) => self.expand_var_in(name, in_quotes),
            Word::Arith(expr) => self.eval_arith_expr(expr),
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

    /// Evaluates the body of a `$(( ))`: `$` references are expanded first,
    /// then bare variable names are read and assigned by the evaluator.
    fn eval_arith_expr(&mut self, expr: &str) -> Result<String> {
        let value = if expr.contains(['$', '`', '\\']) {
            let expanded = self.expand_raw_text(expr)?;
            eval_arith_with(&expanded, &mut self.env)?
        } else {
            eval_arith_with(expr, &mut self.env)?
        };
        Ok(value.to_string())
    }

    /// # Errors
    ///
    /// Returns an error if `name` is unset under `set -u`, or if a nested
    /// command substitution fails.
    pub fn expand_var(&mut self, name: &str) -> Result<String> {
        self.expand_var_in(name, false)
    }

    /// Expands the body of a `$name`/`${...}` reference. `in_quotes` says
    /// whether it sits inside double quotes, which changes how the operand
    /// word of `${var:-word}` and friends is parsed.
    fn expand_var_in(&mut self, name: &str, in_quotes: bool) -> Result<String> {
        if !has_param_op(name) {
            return self.resolve_param_checked(name);
        }
        match parse_param_op(name) {
            Some(ParamOp::Length(var)) => {
                Ok(self.resolve_param_checked(var)?.chars().count().to_string())
            }
            Some(ParamOp::PrefixStrip { var, pat, greedy }) => {
                let val = self.resolve_param_checked(var)?;
                let pat = self.expand_param_pattern(pat, in_quotes)?;
                Ok(strip_prefix(&val, &pat, greedy))
            }
            Some(ParamOp::SuffixStrip { var, pat, greedy }) => {
                let val = self.resolve_param_checked(var)?;
                let pat = self.expand_param_pattern(pat, in_quotes)?;
                Ok(strip_suffix(&val, &pat, greedy))
            }
            Some(ParamOp::Conditional { var, op, word }) => {
                match self.resolve_conditional(var, op, word, in_quotes)? {
                    Conditional::Value(val) => Ok(val),
                    Conditional::Word(word) => self.expand_param_word(word, in_quotes),
                }
            }
            None => self.resolve_param_checked(name),
        }
    }

    /// Decides what `${var<op>word}` expands to. The colon forms (`:-`, `:+`,
    /// `:?`, `:=`) treat an empty `var` like an unset one; the others only
    /// react to unset. Exempt from `nounset`, since every form handles the
    /// unset case itself.
    fn resolve_conditional<'w>(
        &mut self,
        var: &str,
        op: &str,
        word: &'w str,
        in_quotes: bool,
    ) -> Result<Conditional<'w>> {
        let val = self.resolve_param(var);
        let (missing, base) = op.strip_prefix(':').map_or_else(
            || (!self.is_param_set(var), op),
            |base| (val.is_empty(), base),
        );
        Ok(match (base, missing) {
            ("-", true) | ("+", false) => Conditional::Word(word),
            ("+", true) => Conditional::Value(String::new()),
            ("?", true) => {
                let msg = if word.is_empty() {
                    "parameter not set".to_owned()
                } else {
                    self.expand_param_word(word, in_quotes)?
                };
                if self.interactive {
                    return Err(anyhow!("{var}: {msg}"));
                }
                emit(format!("{var}: {msg}"));
                self.exit(1);
            }
            ("=", true) => {
                let expanded = self.expand_param_word(word, in_quotes)?;
                self.env.assign(var, &expanded)?;
                Conditional::Value(expanded)
            }
            _ => Conditional::Value(val),
        })
    }

    /// Expands the operand of `${var:-word}` and friends as one string.
    fn expand_param_word(&mut self, raw: &str, in_quotes: bool) -> Result<String> {
        if is_plain_operand(raw) {
            return Ok(raw.to_owned());
        }
        let word = parse_word(raw, in_quotes, Some(&self.env.aliases()))?;
        if in_quotes {
            self.expand_word_to_string_inner(&word, true)
        } else {
            self.expand_word_to_string(&word)
        }
    }

    /// Expands the pattern of `${var#pattern}` and friends. The pattern stays
    /// active even inside double quotes; only its own quoted parts are literal.
    fn expand_param_pattern(&mut self, raw: &str, in_quotes: bool) -> Result<String> {
        if is_plain_operand(raw) {
            return Ok(raw.to_owned());
        }
        let word = parse_word(raw, in_quotes, Some(&self.env.aliases()))?;
        self.expand_pattern(&word)
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
            "-" => self.option_flags(),
            "0" => self.arg0.clone(),
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
            "?" | "!" | "$" | "0" | "@" | "*" | "#" | "-" => true,
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
    fn resolve_param_checked(&mut self, name: &str) -> Result<String> {
        if self.nounset && !self.is_param_set(name) {
            if self.interactive {
                anyhow::bail!("{name}: unbound variable");
            }
            emit(format!("{name}: unbound variable"));
            self.exit(if self.errexit { 1 } else { 127 });
        }
        Ok(self.resolve_param(name))
    }

    fn expand_cmd_sub(&mut self, cmd: &Command) -> Result<String> {
        let (read_fd, write_fd) = raw_pipe()?;
        // SAFETY: fork.
        match unsafe { fork()? } {
            Fork::Child => {
                // SAFETY: in child, before any allocations.
                unsafe { restore_child_signals(self.interactive || self.monitor) };
                close_raw(read_fd);
                let _ = dup2_raw(write_fd, 1);
                close_raw(write_fd);
                // The forked copy of this shell is the subshell, options and
                // all, minus its traps and job control.
                self.reset_traps_for_subshell();
                let status = match self.run_command(cmd) {
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
    /// value of `$ENV` at startup. As inside double quotes, a backslash
    /// escapes only `$`, `` ` ``, `\` and newline; before anything else it
    /// stays literal.
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
                    result.push_str(&self.expand_var_in(&var, true)?);
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
                        result.push_str(&self.eval_arith_expr(&cmd_src)?);
                    } else if let Ok(program) = parse(&cmd_src)
                        && let Ok(s) = self.expand_cmd_sub(&program.into_command())
                    {
                        result.push_str(&s);
                    }
                }
                Some(c2) if c2.is_ascii_alphanumeric() || c2 == '_' || "@*#?-$!".contains(c2) => {
                    if "@*#?-$!".contains(c2) {
                        chars.next();
                        result.push_str(&self.expand_var_in(&c2.to_string(), true)?);
                    } else {
                        var.push_str(&take_identifier(&mut chars));
                        result.push_str(&self.expand_var_in(&var, true)?);
                    }
                }
                _ => result.push('$'),
            }
        }
        Ok(result)
    }
}

/// What a `${var<op>word}` reference resolved to.
enum Conditional<'w> {
    Value(String),
    /// The operand word, still to be expanded.
    Word(&'w str),
}

/// Fields built up while expanding a list of words (POSIX 2.6.5, 2.6.6).
struct Fields<'a> {
    ifs: &'a str,
    out: Vec<String>,
    cur: String,
    /// The current field contains a quoted part, so it is kept even if empty.
    quoted: bool,
    /// The current field contains an unquoted `*`, `?` or `[`.
    glob: bool,
    /// `set -f`: never pathname-expand.
    noglob: bool,
    /// The current field as a glob pattern, with its quoted `*?[\` escaped.
    /// Only built once one of those is quoted; until then it equals `cur`.
    pattern: Option<String>,
}

impl<'a> Fields<'a> {
    fn new(ifs: &'a str, capacity: usize) -> Self {
        Self {
            ifs,
            out: Vec::with_capacity(capacity),
            cur: String::new(),
            quoted: false,
            glob: false,
            noglob: false,
            pattern: None,
        }
    }

    /// Nothing has been added to the current field yet, so a `~` here
    /// begins a tilde prefix.
    const fn at_word_start(&self) -> bool {
        self.cur.is_empty() && !self.quoted
    }

    fn push_quoted(&mut self, s: &str) {
        if self.pattern.is_none() && s.contains(['*', '?', '[', '\\']) {
            self.pattern = Some(escape_glob(&self.cur));
        }
        if let Some(pattern) = &mut self.pattern {
            pattern.push_str(&escape_glob(s));
        }
        self.cur.push_str(s);
        self.quoted = true;
    }

    fn push_unquoted(&mut self, s: &str) {
        self.glob |= s.contains(['*', '?', '[']);
        if let Some(pattern) = &mut self.pattern {
            pattern.push_str(s);
        }
        self.cur.push_str(s);
    }

    fn push_unquoted_char(&mut self, c: char) {
        self.glob |= matches!(c, '*' | '?' | '[');
        if let Some(pattern) = &mut self.pattern {
            pattern.push(c);
        }
        self.cur.push(c);
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
                self.push_unquoted_char(c);
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
        let pattern = self.pattern.take();
        if force || self.quoted || !self.cur.is_empty() {
            let field = std::mem::take(&mut self.cur);
            let pattern = pattern.as_deref().unwrap_or(&field);
            let matches = if self.glob && !self.noglob && has_glob_syntax(pattern) {
                glob_expand(pattern)
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

/// Whether a `${var...}` operand is text that parsing would leave as it is:
/// nothing quoted, expanded, escaped or tilde-prefixed.
fn is_plain_operand(raw: &str) -> bool {
    !raw.contains(['$', '`', '\\', '\'', '"', '~', QUOTE_START, QUOTE_END])
}

/// Whether `name`, the text of a `$name` or `${...}`, could hold an
/// operator (`${#v}`, `${v:-w}`, ...) rather than just naming a parameter.
fn has_param_op(name: &str) -> bool {
    name.len() > 1 && !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
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
