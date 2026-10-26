use crate::sys::{Fork, fork};
use anyhow::{Result, anyhow};
use rustix::io::Errno;
use rustix::process::{WaitOptions, waitpid};

use crate::ast::{Command, Word};
use crate::errfmt::emit;
use crate::expand::{
    GlobOptions, ParamOp, escape_glob, eval_arith_with, expand_tilde, glob_expand, has_glob_syntax,
    parse_param_op,
};
use crate::fd::{close_raw, dup2_raw, raw_pipe, read_raw};
use crate::jobs::ExitStatus;
use crate::lexer::{QUOTE_END, QUOTE_START};
use crate::parser::{parse, parse_word};
use crate::signal::restore_child_signals;

use super::param::{Conditional, Expansion};
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
            let matches = glob_expand(&raw, self.glob_options());
            if !matches.is_empty() {
                return Ok(matches);
            }
        }
        Ok(vec![raw])
    }

    /// The `shopt` options that change pathname expansion.
    pub(super) const fn glob_options(&self) -> GlobOptions {
        GlobOptions {
            dotglob: self.shopt.dotglob,
            nocase: self.shopt.nocaseglob,
            globstar: self.shopt.globstar,
        }
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
        fields.globbing = Globbing {
            noglob: self.noglob,
            options: self.glob_options(),
            nullglob: self.shopt.nullglob,
            failglob: self.shopt.failglob,
        };
        for word in words {
            self.expand_into_fields(word, false, &mut fields)?;
            fields.finish(false);
        }
        if let Some(pattern) = fields.unmatched {
            return Err(anyhow!("no match: {pattern}"));
        }
        Ok(fields.out)
    }

    /// [`Self::expand_words`] for a command's words. The assignment words
    /// among the arguments of a declaration command are expanded as
    /// assignments and handed over as markers (see [`Shell::declaration`]).
    pub(super) fn expand_command_words(&mut self, words: &[Word]) -> Result<Vec<String>> {
        if !words.iter().any(is_assignment_word) {
            return self.expand_words(words);
        }
        self.decl_assigns.clear();
        let mut out = Vec::with_capacity(words.len());
        for word in words {
            if let Word::Assign(assign) = word {
                let expanded = self.expand_assign(assign)?;
                out.push(self.mark_declaration(expanded));
            } else {
                out.extend(self.expand_words(std::slice::from_ref(word))?);
            }
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
        push_list(self.env.positional_args(), star, in_quotes, fields);
    }

    fn push_expansion(expansion: Expansion, in_quotes: bool, fields: &mut Fields<'_>) {
        match expansion {
            Expansion::One(value) if in_quotes => fields.push_quoted(&value),
            Expansion::One(value) => fields.push_split(&value),
            Expansion::List { items, star } => push_list(&items, star, in_quotes, fields),
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
            Word::Var(name) if !has_param_op(name) => {
                let value = self.resolve_param_checked(name)?;
                if in_quotes {
                    fields.push_quoted(&value);
                } else {
                    fields.push_split(&value);
                }
            }
            Word::Var(name) => {
                let op =
                    parse_param_op(name).ok_or_else(|| anyhow!("${{{name}}}: bad substitution"))?;
                let ParamOp::Conditional { var, op, word } = op else {
                    let expansion = self.expand_param_op(op, in_quotes)?;
                    Self::push_expansion(expansion, in_quotes, fields);
                    return Ok(());
                };
                match self.resolve_conditional(&var, op, word, in_quotes)? {
                    Conditional::Value(expansion) => {
                        Self::push_expansion(expansion, in_quotes, fields);
                    }
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
            }
            Word::ProcSub { cmd, output } => {
                let path = self.expand_proc_sub(cmd, *output)?;
                fields.push_quoted(&path);
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
            Word::Assign(assign) => {
                let text = self.expand_assign(assign)?.display();
                fields.push_quoted(&text);
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

    pub(super) fn expand_word_to_string_inner(
        &mut self,
        word: &Word,
        in_quotes: bool,
    ) -> Result<String> {
        match word {
            Word::Literal(s) => Ok(s.clone()),
            Word::Var(name) => self.expand_var_in(name, in_quotes),
            Word::Arith(expr) => self.eval_arith_expr(expr),
            Word::CmdSub(cmd) => self.expand_cmd_sub(cmd),
            Word::ProcSub { cmd, output } => self.expand_proc_sub(cmd, *output),
            Word::Compound(parts) => {
                let mut result = String::new();
                for part in parts {
                    result.push_str(&self.expand_word_to_string_inner(part, in_quotes)?);
                }
                Ok(result)
            }
            Word::Quoted(inner) => self.expand_word_to_string_inner(inner, true),
            Word::Assign(assign) => Ok(self.expand_assign(assign)?.display()),
        }
    }

    /// Evaluates the body of a `$(( ))`: `$` references are expanded first,
    /// then bare variable names are read and assigned by the evaluator.
    fn eval_arith_expr(&mut self, expr: &str) -> Result<String> {
        Ok(self.eval_arith_value(expr)?.to_string())
    }

    /// [`Self::eval_arith_expr`]'s value as a number.
    pub(super) fn eval_arith_value(&mut self, expr: &str) -> Result<i64> {
        Ok(if expr.contains(['$', '`', '\\']) {
            let expanded = self.expand_raw_text(expr)?;
            eval_arith_with(&expanded, &mut self.env)?
        } else {
            eval_arith_with(expr, &mut self.env)?
        })
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
                if !self.shopt.inherit_errexit {
                    self.errexit = false;
                }
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
                if let Ok(Some((_, status))) = waitpid(Some(child), WaitOptions::empty()) {
                    self.cmdsub_status = crate::jobs::decode_wait_status(status);
                }
                Ok(String::from_utf8_lossy(&output)
                    .trim_end_matches('\n')
                    .to_owned())
            }
        }
    }

    /// `<(cmd)` or `>(cmd)`: starts `cmd` with its output going into a
    /// pipe, or its input coming from one, and names the shell's end of it
    /// as `/dev/fd/N`. The descriptor stays open until the command using
    /// the name finishes (see `Shell::run_command`).
    fn expand_proc_sub(&mut self, cmd: &Command, output: bool) -> Result<String> {
        let (read_fd, write_fd) = raw_pipe()?;
        let (ours, theirs, target) = if output {
            (write_fd, read_fd, 0)
        } else {
            (read_fd, write_fd, 1)
        };
        // SAFETY: fork.
        match unsafe { fork()? } {
            Fork::Child => {
                // SAFETY: in child, before any allocations.
                unsafe { restore_child_signals(self.interactive || self.monitor) };
                close_raw(ours);
                let _ = dup2_raw(theirs, target);
                close_raw(theirs);
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
                close_raw(theirs);
                self.proc_subs.push(ours);
                self.last_bg_pid = Some(child);
                Ok(format!("/dev/fd/{ours}"))
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
        let mut chars = text.char_indices().peekable();
        while let Some((_, c)) = chars.next() {
            if c == '\\' {
                match chars.next_if(|&(_, n)| matches!(n, '$' | '`' | '\\' | '\n')) {
                    Some((_, '\n')) => {}
                    Some((_, escaped)) => result.push(escaped),
                    None => result.push('\\'),
                }
                continue;
            }
            if c != '$' {
                result.push(c);
                continue;
            }
            match chars.peek().copied() {
                Some((open, brace @ ('{' | '('))) => {
                    let start = open + 1;
                    let rest = &text.as_bytes()[start..];
                    let len = if brace == '{' {
                        crate::lexer::param_len(rest, true)
                    } else {
                        crate::lexer::cmd_sub_len(rest)
                    };
                    let end = start + len.unwrap_or(rest.len());
                    while chars.next_if(|&(i, _)| i <= end).is_some() {}
                    let inner = &text[start..end];
                    if brace == '{' {
                        result.push_str(&self.expand_var_in(inner, true)?);
                    } else if inner.starts_with('(')
                        && end < text.len()
                        && crate::lexer::is_arith(&text.as_bytes()[start + 1..=end])
                    {
                        result.push_str(&self.eval_arith_expr(&inner[1..inner.len() - 1])?);
                    } else if let Ok(program) = parse(inner)
                        && let Ok(s) = self.expand_cmd_sub(&program.into_command())
                    {
                        result.push_str(&s);
                    }
                }
                Some((at, c2)) if "@*#?-$!".contains(c2) => {
                    chars.next();
                    result.push_str(&self.expand_var_in(&text[at..=at], true)?);
                }
                Some((at, c2)) if c2.is_ascii_alphanumeric() || c2 == '_' => {
                    let mut end = at;
                    while let Some((i, c)) =
                        chars.next_if(|&(_, c)| c.is_ascii_alphanumeric() || c == '_')
                    {
                        end = i + c.len_utf8();
                    }
                    result.push_str(&self.expand_var_in(&text[at..end], true)?);
                }
                _ => result.push('$'),
            }
        }
        Ok(result)
    }
}

/// How [`Fields`] does pathname expansion.
#[derive(Default)]
struct Globbing {
    /// `set -f`: never pathname-expand.
    noglob: bool,
    /// `shopt` options for pathname expansion.
    options: GlobOptions,
    /// `shopt nullglob`: a pattern matching nothing gives no field.
    nullglob: bool,
    /// `shopt failglob`: a pattern matching nothing is an error.
    failglob: bool,
}

/// Fields built up while expanding a list of words (POSIX 2.6.5, 2.6.6).
struct Fields<'a> {
    ifs: &'a str,
    out: Vec<String>,
    cur: String,
    /// The current field contains a quoted part, so it is kept even if empty.
    quoted: bool,
    /// The current field contains an unquoted `*`, `?`, `[` or `(`.
    glob: bool,
    /// When and how pathname expansion happens.
    globbing: Globbing,
    /// The first pattern that matched nothing, under `failglob`.
    unmatched: Option<String>,
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
            globbing: Globbing::default(),
            unmatched: None,
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
        // `(` for an extended pattern such as `@(a|b)`.
        self.glob |= s.contains(['*', '?', '[', '(']);
        if let Some(pattern) = &mut self.pattern {
            pattern.push_str(s);
        }
        self.cur.push_str(s);
    }

    fn push_unquoted_char(&mut self, c: char) {
        self.glob |= matches!(c, '*' | '?' | '[' | '(');
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
            let globbed = self.glob && !self.globbing.noglob && has_glob_syntax(pattern);
            let matches = if globbed {
                glob_expand(pattern, self.globbing.options)
            } else {
                Vec::new()
            };
            if !matches.is_empty() {
                self.out.extend(matches);
            } else if globbed && self.globbing.failglob {
                self.unmatched.get_or_insert(field);
            } else if !(globbed && self.globbing.nullglob) {
                self.out.push(field);
            }
        }
        self.quoted = false;
        self.glob = false;
    }
}

/// Whether a `${var...}` operand is text that parsing would leave as it is:
/// nothing quoted, expanded, escaped or tilde-prefixed.
pub(super) fn is_plain_operand(raw: &str) -> bool {
    !raw.contains(['$', '`', '\\', '\'', '"', '~', QUOTE_START, QUOTE_END])
}

/// Whether `name`, the text of a `$name` or `${...}`, could hold an
/// operator (`${#v}`, `${v:-w}`, ...) rather than just naming a parameter.
pub(super) fn has_param_op(name: &str) -> bool {
    name.len() > 1 && !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Puts each of `items` in a field of its own, as `"$@"` does; or with
/// `star` and `in_quotes`, all in one field, separated by the first
/// character of `$IFS`, as `"$*"` does.
fn push_list(items: &[String], star: bool, in_quotes: bool, fields: &mut Fields<'_>) {
    if in_quotes && star {
        let sep = fields
            .ifs
            .chars()
            .next()
            .map(String::from)
            .unwrap_or_default();
        fields.push_quoted(&items.join(&sep));
        return;
    }
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            fields.finish(in_quotes);
        }
        if in_quotes {
            fields.push_quoted(item);
        } else {
            fields.push_split(item);
        }
    }
}

/// Whether `word` holds a `"$@"`-like expansion, which inside double
/// quotes gives no field at all when it has no items.
fn contains_at(word: &Word) -> bool {
    match word {
        Word::Var(name) => {
            name == "@"
                || (name.contains('@') && parse_param_op(name).is_some_and(|op| op.is_at_list()))
        }
        Word::Compound(parts) => parts.iter().any(contains_at),
        Word::Quoted(inner) => contains_at(inner),
        _ => false,
    }
}
