use std::fmt;
use std::rc::Rc;

use crate::ast::{
    AndOrItem, AndOrList, AndOrOp, ArrayItem, Assign, AssignValue, Command, Pipeline, Program,
    Redirect, RedirectKind, SimpleCmd, TimeFormat, Word,
};
use crate::expand::{find_unnested, is_name};
use crate::lexer::{
    ArrayLiteral, LexError, Lexer, PROC_SUB, QUOTE_END, QUOTE_END_BYTE, QUOTE_START,
    QUOTE_START_BYTE, RedirKind, RedirToken, Token,
};

mod compound;

// ---------------------------------------------------------------------------
// Parse errors
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ParseError {
    pub line: usize,
    pub col: usize,
    pub msg: String,
    /// Set when the parser ran out of tokens mid-construct (an unclosed
    /// `if`/`while`/`for`/`case`/`{`/`(`, or an unterminated quote or
    /// substitution from the lexer) rather than hitting a genuinely wrong
    /// token. The REPL uses this to decide whether to prompt for more
    /// input (`> `) instead of reporting a hard error.
    pub incomplete: bool,
    /// The input ended inside a here-document body (only reported by
    /// [`parse_with_aliases`]), so the next line belongs to the body as it
    /// stands rather than being shell syntax.
    pub in_heredoc: bool,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "parse error at {}:{}: {}", self.line, self.col, self.msg)
    }
}

impl std::error::Error for ParseError {}

impl From<LexError> for ParseError {
    fn from(e: LexError) -> Self {
        Self {
            line: e.line,
            col: e.col,
            msg: e.msg.to_owned(),
            incomplete: e.incomplete,
            in_heredoc: false,
        }
    }
}

type ParseResult<T> = Result<T, ParseError>;

// ---------------------------------------------------------------------------
// Spanned token
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Spanned {
    token: Token,
    line: usize,
    col: usize,
    /// Byte offset just past this token in the source, for `set -v`.
    end: usize,
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

/// Alias definitions, by name, as the parser sees them.
pub type Aliases = crate::env::AliasMap;

/// What word parsing carries into the parsers it starts for `$(...)` and
/// backtick bodies: the nesting depth so far, and the aliases to expand.
#[derive(Clone, Copy)]
struct Nest<'a> {
    depth: usize,
    aliases: Option<&'a Rc<Aliases>>,
}

impl Nest<'_> {
    const fn deeper(self) -> Self {
        Self {
            depth: self.depth + 1,
            aliases: self.aliases,
        }
    }
}

/// How far ahead of `pos` the token buffer is kept filled, which is as far
/// as any lookahead in the grammar reaches (`is_function_def_ahead`).
const LOOKAHEAD: usize = 1;

/// The commands whose `name=value` arguments are assignment words.
const DECLARATION_COMMANDS: [&str; 5] = ["declare", "export", "local", "readonly", "typeset"];

/// A parser that pulls tokens from the lexer only as the grammar needs them.
///
/// That lets a script be parsed and run one complete command at a time
/// ([`Parser::next_command`]), so aliases defined by one command apply to
/// the next.
pub struct Parser<'src> {
    lexer: Lexer<'src>,
    /// The buffer ends in `Token::Eof`, real or standing in for `lex_error`.
    lexed_eof: bool,
    /// A lex error, and the index of the `Token::Eof` pushed in its place.
    /// Reported by whichever parse first reaches that index, so commands
    /// before the bad text still parse (and run) normally.
    lex_error: Option<(usize, ParseError)>,
    tokens: Vec<Spanned>,
    pos: usize,
    /// Indices of `Token::HereDoc`s whose bodies the lexer has not read yet.
    unfilled_heredocs: Vec<usize>,
    /// Current `parse_command` nesting depth; see `MAX_PARSE_DEPTH`.
    depth: usize,
    aliases: Option<Rc<Aliases>>,
    /// `(name, start, end)` token ranges produced by expanding alias `name`,
    /// so an alias is not expanded again inside its own expansion.
    alias_spans: Vec<(String, usize, usize)>,
    /// The token after an alias whose value ends in a blank, which POSIX
    /// says is checked for alias expansion too.
    alias_next: Option<usize>,
}

/// Every level of nested compound command (`{ }`, `( )`, `if`/`while`/...)
/// recurses through `parse_command` exactly once, so bounding this bounds
/// the whole `parse_command` -> `parse_pipeline` -> `parse_and_or` ->
/// `parse_list` -> `parse_command` cycle (found unbounded by fuzzing, see
/// fuzz/README.md: a few hundred levels of `{ }` nesting overflowed the
/// native stack).
/// A power of two, both well below the observed crash point (a few
/// hundred levels) and far deeper than any script would plausibly nest by
/// hand.
const MAX_PARSE_DEPTH: usize = 256;

impl<'src> Parser<'src> {
    #[must_use]
    pub fn new(src: &'src str) -> Self {
        Self::with_aliases(src, None)
    }

    /// [`Parser::new`], expanding the given aliases while parsing.
    #[must_use]
    pub fn with_aliases(src: &'src str, aliases: Option<Rc<Aliases>>) -> Self {
        Self::new_at_depth(src, 0, aliases)
    }

    /// `Parser::new`, but starting the nesting counter at `depth` instead of
    /// zero.
    ///
    /// A `$(...)` or backtick substitution is parsed by handing its interior
    /// to a *new* `Parser`. Carrying the depth across that boundary makes
    /// `MAX_PARSE_DEPTH` bound the real recursion, which crosses parser
    /// instances: otherwise a few thousand nested `$(` would overflow the
    /// stack even though the identical depth of `{ }` is rejected cleanly.
    fn new_at_depth(src: &'src str, depth: usize, aliases: Option<Rc<Aliases>>) -> Self {
        let mut parser = Self {
            lexer: Lexer::new(src),
            lexed_eof: false,
            lex_error: None,
            tokens: Vec::new(),
            pos: 0,
            unfilled_heredocs: Vec::new(),
            depth,
            aliases,
            alias_spans: Vec::new(),
            alias_next: None,
        };
        parser.fill();
        parser
    }

    /// Lexes until the buffer holds `LOOKAHEAD` tokens past `pos`, or ends in
    /// `Token::Eof`, filling in here-doc bodies as the lexer reads them.
    fn fill(&mut self) {
        while !self.lexed_eof && self.tokens.len() <= self.pos + LOOKAHEAD {
            self.lex_one();
        }
    }

    /// A here-doc's body follows the next newline, which can be well past
    /// the lookahead; lexes ahead until the one at `idx` has it.
    fn read_heredoc_body_at(&mut self, idx: usize) {
        while !self.lexed_eof && self.unfilled_heredocs.contains(&idx) {
            self.lex_one();
        }
    }

    fn lex_one(&mut self) {
        let (line, col) = self.lexer.position();
        let token = match self.lexer.next_token() {
            Ok(token) => token,
            Err(e) => {
                self.lex_error = Some((self.tokens.len(), e.into()));
                Token::Eof
            }
        };
        self.lexed_eof = matches!(token, Token::Eof);
        if matches!(token, Token::HereDoc { .. }) {
            self.unfilled_heredocs.push(self.tokens.len());
        }
        self.tokens.push(Spanned {
            token,
            line,
            col,
            end: self.lexer.offset(),
        });
        let bodies = self.lexer.take_heredoc_bodies();
        for (idx, body) in self.unfilled_heredocs.drain(..bodies.len()).zip(bodies) {
            if let Token::HereDoc { body: slot, .. } = &mut self.tokens[idx].token {
                *slot = body;
            }
        }
    }

    /// The pending lex error, if parsing has reached the point where it was
    /// found.
    fn reached_lex_error(&self) -> Option<&ParseError> {
        self.lex_error
            .as_ref()
            .filter(|(idx, _)| self.pos >= *idx)
            .map(|(_, e)| e)
    }

    /// Replaces the word at `pos` with the tokens of the alias it names, if
    /// it names one that is not already being expanded at this point.
    /// Returns whether it did.
    fn expand_alias(&mut self) -> ParseResult<bool> {
        let pos = self.pos;
        let Token::Word(name) = &self.tokens[pos].token else {
            return Ok(false);
        };
        let Some(value) = self.aliases.as_ref().and_then(|a| a.get(name)) else {
            return Ok(false);
        };
        self.alias_spans.retain(|(_, _, end)| *end > pos);
        if self
            .alias_spans
            .iter()
            .any(|(active, start, end)| active == name && (*start..*end).contains(&pos))
        {
            return Ok(false);
        }
        let (name, value) = (name.clone(), value.clone());
        let Spanned { line, col, end, .. } = self.tokens[pos];
        let lexed = lex_alias_value(&value)?;
        let replacement: Vec<Spanned> = lexed
            .tokens
            .into_iter()
            .map(|token| Spanned {
                token,
                line,
                col,
                end,
            })
            .collect();
        let added = replacement.len();
        self.tokens.splice(pos..=pos, replacement);
        let shift = |i: &mut usize| {
            if *i > pos {
                *i = *i + added - 1;
            }
        };
        for (_, _, span_end) in &mut self.alias_spans {
            shift(span_end);
        }
        for idx in &mut self.unfilled_heredocs {
            shift(idx);
        }
        if let Some((idx, _)) = &mut self.lex_error {
            shift(idx);
        }
        // Here-docs in the value take their bodies from the input after the
        // next newline, in token order with any already waiting there.
        let first = self.unfilled_heredocs.iter().filter(|&&i| i < pos).count();
        for (at, (offset, (delim, strip_tabs))) in (first..).zip(lexed.pending) {
            self.unfilled_heredocs.insert(at, pos + offset);
            self.lexer.insert_pending_heredoc(at, delim, strip_tabs);
        }
        self.alias_spans.push((name, pos, pos + added));
        if value.ends_with([' ', '\t']) {
            self.alias_next = Some(pos + added);
        } else if let Some(next) = &mut self.alias_next {
            shift(next);
        }
        self.fill();
        Ok(true)
    }

    // ------------------------------------------------------------------
    // Cursor helpers
    // ------------------------------------------------------------------

    #[inline]
    fn peek(&self) -> &Token {
        &self.tokens[self.pos].token
    }

    #[inline]
    fn peek_spanned(&self) -> &Spanned {
        &self.tokens[self.pos]
    }

    /// Moves past the current token, returning it. Every way `pos` moves
    /// goes through here, which keeps the lookahead buffer filled.
    fn advance(&mut self) -> &Token {
        let current = self.pos;
        if self.pos + 1 < self.tokens.len() {
            self.pos += 1;
        }
        self.fill();
        &self.tokens[current].token
    }

    fn expect(&mut self, expected: &Token) -> ParseResult<()> {
        if self.peek() == expected {
            self.advance();
            Ok(())
        } else {
            if let Some(e) = self.reached_lex_error() {
                return Err(e.clone());
            }
            let sp = self.peek_spanned();
            Err(ParseError {
                line: sp.line,
                col: sp.col,
                msg: format!("expected `{expected}`, got `{}`", sp.token),
                incomplete: sp.token == Token::Eof,
                in_heredoc: false,
            })
        }
    }

    /// The token the error points at ran out (`Token::Eof`): the grammar
    /// wanted more but the input simply ended, e.g. `if true` with no
    /// `then` yet. Every hard syntax error instead points at a real,
    /// unexpected token, so this check alone is enough to tell the two
    /// apart everywhere `err`/`expect` are used.
    fn err(&self, msg: impl Into<String>) -> ParseError {
        if let Some(e) = self.reached_lex_error() {
            return e.clone();
        }
        let sp = self.peek_spanned();
        ParseError {
            line: sp.line,
            col: sp.col,
            msg: msg.into(),
            incomplete: sp.token == Token::Eof,
            in_heredoc: false,
        }
    }

    fn skip_newlines(&mut self) {
        while matches!(self.peek(), Token::Newline | Token::Semi) {
            self.advance();
        }
    }

    fn eat(&mut self, tok: &Token) -> bool {
        if self.peek() == tok {
            self.advance();
            true
        } else {
            false
        }
    }

    // ------------------------------------------------------------------
    // Top-level entry point
    // ------------------------------------------------------------------

    /// # Errors
    ///
    /// Returns an error if the token stream doesn't form a valid program.
    pub fn parse(mut self) -> ParseResult<Program> {
        let result = self.parse_program();
        if self.lexer.heredoc_ran_out() {
            let (line, col) = self.lexer.position();
            return Err(ParseError {
                line,
                col,
                msg: "here-document not terminated".to_owned(),
                incomplete: true,
                in_heredoc: true,
            });
        }
        result
    }

    fn parse_program(&mut self) -> ParseResult<Program> {
        let body = self.parse_list()?;
        if *self.peek() != Token::Eof {
            return Err(self.err(format!("unexpected token `{}`", self.peek())));
        }
        if let Some(e) = self.reached_lex_error() {
            return Err(e.clone());
        }
        Ok(Program { body })
    }

    /// Parses the next complete command: everything up to the end of its
    /// line, which may hold several `;`- or `&`-separated lists, or span
    /// lines inside a compound command. `None` at end of input.
    ///
    /// # Errors
    ///
    /// Returns an error if that command is malformed.
    pub fn next_command(&mut self, aliases: Option<Rc<Aliases>>) -> ParseResult<Option<Program>> {
        self.aliases = aliases;
        self.discard_consumed();
        while matches!(self.peek(), Token::Newline) {
            self.advance();
        }
        if *self.peek() == Token::Eof {
            return self
                .reached_lex_error()
                .map_or(Ok(None), |e| Err(e.clone()));
        }
        let mut body = Vec::new();
        loop {
            body.push(self.parse_and_or()?);
            match self.peek() {
                Token::Newline => {
                    self.advance();
                    break;
                }
                Token::Eof => break,
                _ if self.at_list_terminator() => {
                    return Err(self.err(format!("unexpected token `{}`", self.peek())));
                }
                _ => {}
            }
        }
        if let Some(e) = self.reached_lex_error() {
            return Err(e.clone());
        }
        Ok(Some(Program { body }))
    }

    /// The source consumed by the last [`Parser::next_command`], up to the
    /// byte offset this returns.
    #[must_use]
    pub fn consumed_offset(&self) -> usize {
        self.pos
            .checked_sub(1)
            .and_then(|i| self.tokens.get(i))
            .map_or(0, |t| t.end)
    }

    /// Drops tokens already parsed, so a long script's buffer does not grow
    /// without bound.
    fn discard_consumed(&mut self) {
        let n = self.pos.saturating_sub(1);
        if n == 0 {
            return;
        }
        self.tokens.drain(..n);
        self.pos -= n;
        for idx in &mut self.unfilled_heredocs {
            *idx -= n;
        }
        self.alias_spans.retain(|(_, _, end)| *end > n);
        for (_, start, end) in &mut self.alias_spans {
            *start = start.saturating_sub(n);
            *end -= n;
        }
        self.alias_next = self.alias_next.and_then(|i| i.checked_sub(n));
        if let Some((idx, _)) = &mut self.lex_error {
            *idx = idx.saturating_sub(n);
        }
    }

    // ------------------------------------------------------------------
    // List  ::=  (NewlineList | AndOrList (';' | '&' | '\n')*)*
    // ------------------------------------------------------------------

    fn parse_list(&mut self) -> ParseResult<Vec<AndOrList>> {
        let mut list = Vec::new();
        self.skip_newlines();

        loop {
            if self.at_list_terminator() {
                break;
            }
            let aol = self.parse_and_or()?;
            list.push(aol);
            self.skip_newlines();
        }

        Ok(list)
    }

    fn at_list_terminator(&self) -> bool {
        matches!(
            self.peek(),
            Token::Eof
                | Token::Fi
                | Token::Done
                | Token::Esac
                | Token::Elif
                | Token::Else
                | Token::Then
                | Token::Do
                | Token::RParen
                | Token::RBrace
                | Token::In
                | Token::SemiSemi
                | Token::SemiAmp
                | Token::SemiSemiAmp
        )
    }

    // ------------------------------------------------------------------
    // AndOrList  ::=  Pipeline (('&&' | '||') Newlines Pipeline)* ('&'|';')?
    // ------------------------------------------------------------------

    fn parse_and_or(&mut self) -> ParseResult<AndOrList> {
        // Almost always exactly one: a bare `&&`/`||` chain is the
        // uncommon case. Sized to that instead of `Vec::new()`'s capacity
        // 0, which over-allocates relative to the common single-item case
        // on its first growth.
        let mut items = Vec::with_capacity(1);
        let pipeline = self.parse_pipeline()?;

        let first_op = self.peek_and_or_op();
        items.push(AndOrItem {
            command: pipeline,
            op: first_op,
        });

        if first_op.is_some() {
            self.advance();
            self.skip_newlines();
            loop {
                let pipeline = self.parse_pipeline()?;
                let op = self.peek_and_or_op();
                items.push(AndOrItem {
                    command: pipeline,
                    op,
                });
                if op.is_some() {
                    self.advance();
                    self.skip_newlines();
                } else {
                    break;
                }
            }
        }

        let is_async = if self.eat(&Token::Ampersand) {
            true
        } else {
            self.eat(&Token::Semi);
            false
        };

        Ok(AndOrList { items, is_async })
    }

    fn peek_and_or_op(&self) -> Option<AndOrOp> {
        match self.peek() {
            Token::AndAnd => Some(AndOrOp::And),
            Token::OrOr => Some(AndOrOp::Or),
            _ => None,
        }
    }

    // ------------------------------------------------------------------
    // Pipeline  ::=  '!'? Command ('|' Newlines Command)*
    // ------------------------------------------------------------------

    fn parse_pipeline(&mut self) -> ParseResult<Pipeline> {
        // `time` is reserved only here, at the start of a pipeline.
        let timed = if matches!(self.peek(), Token::Word(w) if w == "time") {
            self.advance();
            if matches!(self.peek(), Token::Word(w) if w == "-p") {
                self.advance();
                Some(TimeFormat::Posix)
            } else {
                Some(TimeFormat::Default)
            }
        } else {
            None
        };
        let negated = self.eat(&Token::Bang);
        // Almost always exactly one: most commands aren't part of a `|`
        // pipeline. See `parse_and_or`'s `items` for the same reasoning.
        let mut commands = Vec::with_capacity(1);

        commands.push(self.parse_command()?);

        while self.eat(&Token::Pipe) {
            self.skip_newlines();
            commands.push(self.parse_command()?);
        }

        Ok(Pipeline {
            commands,
            negated,
            timed,
        })
    }

    // ------------------------------------------------------------------
    // Command  ::=  CompoundCommand | FunctionDef | SimpleCommand
    // ------------------------------------------------------------------

    // Depth-limited wrapper around the real parser below: see
    // `MAX_PARSE_DEPTH`. Found by fuzzing `parser::parse` (see
    // fuzz/README.md) as an uncontrolled stack-overflow crash on deeply
    // nested compound commands; not a memory-safety bug like the UTF-8
    // escape bug that same fuzzing pass found and fixed elsewhere, just an
    // uncontrolled crash on adversarial/pathological input.
    fn parse_command(&mut self) -> ParseResult<Command> {
        self.depth += 1;
        let result = if self.depth > MAX_PARSE_DEPTH {
            Err(self.err("too deeply nested"))
        } else {
            self.parse_command_inner()
        };
        self.depth -= 1;
        result
    }

    fn parse_command_inner(&mut self) -> ParseResult<Command> {
        while self.expand_alias()? {}
        let compound = match self.peek() {
            Token::If => self.parse_if(),
            Token::For => self.parse_for(),
            Token::ArithCmd(expr) => {
                let expr = expr.clone();
                self.advance();
                Ok(Command::Arith(expr))
            }
            Token::Word(w) if w == "[[" => self.parse_cond(),
            Token::Word(w) if w == "select" && self.is_select_ahead() => self.parse_select(),
            Token::Word(w) if w == "coproc" => return self.parse_coproc(),
            Token::While => self.parse_while(false),
            Token::Until => self.parse_while(true),
            Token::Case => self.parse_case(),
            Token::LBrace => self.parse_brace_group(),
            Token::LParen => self.parse_subshell(),
            Token::Function => return self.parse_function_def(),
            Token::Word(_) if self.is_function_def_ahead() => return self.parse_function_def(),
            _ => return self.parse_simple_cmd().map(Command::Simple),
        }?;
        let mut redirects = Vec::new();
        while matches!(
            self.peek(),
            Token::Redir(_) | Token::HereString(_) | Token::HereDoc { .. }
        ) {
            redirects.push(self.parse_redirect()?);
        }
        Ok(if redirects.is_empty() {
            compound
        } else {
            Command::Redirected(Box::new(compound), redirects)
        })
    }

    fn is_function_def_ahead(&self) -> bool {
        if let Token::Word(_) = self.peek()
            && self.pos + 1 < self.tokens.len()
        {
            return matches!(self.tokens[self.pos + 1].token, Token::LParen);
        }
        false
    }

    // ------------------------------------------------------------------
    // SimpleCommand
    // ------------------------------------------------------------------

    fn parse_simple_cmd(&mut self) -> ParseResult<SimpleCmd> {
        // Never worse than `Vec::new()`'s capacity 0 (anything beyond 1
        // word reallocates regardless of where it started), and a real
        // win for the bare-command/assignment-only case, which is common
        // enough on its own (see `parse_and_or`'s `items` for the same
        // reasoning). `redirects` stays `Vec::new()`: most commands have
        // none at all, so pre-sizing it would waste an allocation outright
        // more often than it would save one.
        let line = self.peek_spanned().line;
        let mut words = Vec::with_capacity(1);
        let mut redirects = Vec::new();
        // Assignment words are recognized before the command name, and
        // after it when it is a declaration command, as in bash.
        let mut assigning = true;
        let mut named = false;

        loop {
            if self.alias_next == Some(self.pos) {
                self.alias_next = None;
                if self.expand_alias()? {
                    // The expansion's own first word is checked again, the
                    // way it would be if the value were re-read as input.
                    while self.expand_alias()? {}
                    continue;
                }
            }
            if let Token::Word(w) = self.peek().clone() {
                self.advance();
                let w = if assigning && w.contains('=') {
                    match self.parse_assign_word(w)? {
                        Ok(assign) => {
                            words.push(Word::Assign(Box::new(assign)));
                            continue;
                        }
                        Err(w) => w,
                    }
                } else {
                    w
                };
                if !named {
                    named = true;
                    assigning = assigning && DECLARATION_COMMANDS.contains(&w.as_str());
                }
                self.push_words(w, &mut words)?;
                continue;
            }
            if matches!(self.peek(), Token::ArrayAssign(_)) {
                if !assigning {
                    return Err(self.err("unexpected `(`"));
                }
                let Token::ArrayAssign(literal) = self.advance().clone() else {
                    continue;
                };
                let ArrayLiteral { head, items } = *literal;
                words.push(Word::Assign(Box::new(
                    self.parse_array_assign(&head, &items)?,
                )));
                continue;
            }
            if matches!(
                self.peek(),
                Token::Redir(_) | Token::HereString(_) | Token::HereDoc { .. }
            ) {
                redirects.push(self.parse_redirect()?);
                continue;
            }
            // A reserved word is only reserved as a command's *first*
            // word; past that it's just text like any other argument
            // (`echo done` prints `done`, it doesn't need a loop to
            // close). `words` non-empty is exactly "not the first word."
            if !words.is_empty()
                && let Some(text) = keyword_text(self.peek())
            {
                let text = text.to_owned();
                self.advance();
                words.push(self.parse_word_str(text)?);
                continue;
            }
            break;
        }

        if words.is_empty() && redirects.is_empty() {
            return Err(self.err("expected a command"));
        }

        Ok(SimpleCmd {
            words,
            redirects,
            line,
        })
    }

    // ------------------------------------------------------------------
    // Word parsing
    // ------------------------------------------------------------------

    /// Parses `raw` into the words its brace expansion makes, onto `words`.
    pub(super) fn push_words(&self, raw: String, words: &mut Vec<Word>) -> ParseResult<()> {
        if !raw.contains('{') {
            words.push(self.parse_word_str(raw)?);
            return Ok(());
        }
        let expanded = crate::brace::expand(&raw)
            .ok_or_else(|| self.err("brace expansion makes too many words"))?;
        for raw in expanded {
            words.push(self.parse_word_str(raw)?);
        }
        Ok(())
    }

    /// Parses `raw` as an assignment word if it is one: a name, perhaps a
    /// subscript, then `=` or `+=`. Hands `raw` back if it is not.
    fn parse_assign_word(&self, mut raw: String) -> ParseResult<Result<Assign, String>> {
        let name_end = raw
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .unwrap_or(raw.len());
        if !is_name(&raw[..name_end]) {
            return Ok(Err(raw));
        }
        let mut rest = name_end;
        let mut index = None;
        if raw[rest..].starts_with('[') {
            let Some(close) = find_unnested(&raw[rest + 1..], |c| c == ']') else {
                return Ok(Err(raw));
            };
            index = Some(rest + 1..rest + 1 + close);
            rest += close + 2;
        }
        let (append, value_start) = if raw[rest..].starts_with("+=") {
            (true, rest + 2)
        } else if raw[rest..].starts_with('=') {
            (false, rest + 1)
        } else {
            return Ok(Err(raw));
        };
        if index.as_ref().is_some_and(std::ops::Range::is_empty) {
            return Err(self.err(format!("{}[]: bad array subscript", &raw[..name_end])));
        }
        let name = raw[..name_end].to_owned();
        let index = index
            .map(|r| self.parse_word_str(raw[r].to_owned()))
            .transpose()?;
        // The value reuses the word's own buffer.
        raw.replace_range(..value_start, "");
        Ok(Ok(Assign {
            name,
            index,
            append,
            value: AssignValue::Scalar(self.parse_word_str(raw)?),
        }))
    }

    /// `name=(items)`, from its lexed head (`name=` or `name+=`) and items.
    // Kept out of line: rare, and inlined it bloats the hot parser
    // frames that call it.
    #[inline(never)]
    fn parse_array_assign(&self, head: &str, raw_items: &[String]) -> ParseResult<Assign> {
        let name = head.trim_end_matches('=');
        let (name, append) = name.strip_suffix('+').map_or((name, false), |n| (n, true));
        let mut items = Vec::with_capacity(raw_items.len());
        for raw in raw_items {
            if let Some(inner) = raw.strip_prefix('[')
                && let Some(close) = find_unnested(inner, |c| c == ']')
            {
                let after = &inner[close + 1..];
                let (append, value) = after
                    .strip_prefix("+=")
                    .map_or_else(|| (false, after.strip_prefix('=')), |v| (true, Some(v)));
                if let Some(value) = value {
                    items.push(ArrayItem {
                        index: Some(self.parse_word_str(inner[..close].to_owned())?),
                        append,
                        value: self.parse_word_str(value.to_owned())?,
                    });
                    continue;
                }
            }
            let mut values = Vec::new();
            self.push_words(raw.clone(), &mut values)?;
            items.extend(values.into_iter().map(|value| ArrayItem {
                index: None,
                append: false,
                value,
            }));
        }
        Ok(Assign {
            name: name.to_owned(),
            index: None,
            append,
            value: AssignValue::Array(items),
        })
    }

    // Takes `raw` by value, not `&str`: every call site already holds an
    // owned `String` at the point it calls this (each already had to
    // clone a token out of `self.tokens` to satisfy the borrow checker
    // around the `self.advance()` right next to it), and the common case
    // here is just handing that same text back inside `Word::Literal`
    // unchanged. Borrowing it and re-`to_owned()`-ing on that path was a
    // second, wholly redundant allocation of the same content; found
    // during a page-fault-heavy parse benchmark (4500+ minor faults
    // parsing a 20k-line script, vs. ~100-150 for bash/dash on the same
    // input) that traced back to exactly this kind of doubled allocation.
    fn parse_word_str(&self, raw: String) -> ParseResult<Word> {
        let bytes = raw.as_bytes();

        if !bytes.iter().any(|&b| {
            matches!(
                b,
                b'$' | b'`' | b'"' | b'\\' | QUOTE_START_BYTE | QUOTE_END_BYTE
            )
        }) {
            return Ok(Word::Literal(raw));
        }

        let nest = Nest {
            depth: self.depth,
            aliases: self.aliases.as_ref(),
        };
        let mut parts = decompose_word(&raw, nest, false)?;
        if parts.len() == 1
            && let Some(part) = parts.pop()
        {
            return Ok(part);
        }
        Ok(Word::Compound(parts))
    }

    // ------------------------------------------------------------------
    // Redirect
    // ------------------------------------------------------------------

    fn parse_redirect(&mut self) -> ParseResult<Redirect> {
        self.read_heredoc_body_at(self.pos);
        match self.advance().clone() {
            Token::Redir(RedirToken { kind, fd, fd_var }) => {
                let target_tok = self.peek().clone();
                let target = match target_tok {
                    Token::Word(w) => {
                        let word = self.parse_word_str(w)?;
                        self.advance();
                        word
                    }
                    ref tok if let Some(text) = keyword_text(tok) => {
                        let word = self.parse_word_str(text.to_owned())?;
                        self.advance();
                        word
                    }
                    _ => return Err(self.err("expected filename after redirection")),
                };

                let (rk, default_fd) = match kind {
                    RedirKind::Out => (RedirectKind::Out, 1),
                    RedirKind::Clobber => (RedirectKind::Clobber, 1),
                    RedirKind::Append => (RedirectKind::Append, 1),
                    RedirKind::In => (RedirectKind::In, 0),
                    RedirKind::ReadWrite => (RedirectKind::ReadWrite, 0),
                    RedirKind::OutFd => (RedirectKind::Dup, 1),
                    RedirKind::InFd => (RedirectKind::Dup, 0),
                    RedirKind::BothOut => (RedirectKind::Both, 1),
                };

                Ok(Redirect {
                    kind: rk,
                    fd: fd.unwrap_or(default_fd),
                    target,
                    fd_var,
                })
            }

            Token::HereDoc { body, quoted } => Ok(Redirect {
                kind: RedirectKind::HereDoc {
                    raw_body: body,
                    quoted,
                },
                fd: 0,
                target: Word::Literal(String::new()),
                fd_var: None,
            }),

            Token::HereString(s) => Ok(Redirect {
                kind: RedirectKind::HereString,
                fd: 0,
                // `<<<`'s operand is an ordinary word (quote removal,
                // parameter/command substitution, ...), not a heredoc body:
                // `parse_word_str` is what every other redirect target
                // above already goes through for exactly that reason.
                target: self.parse_word_str(s)?,
                fd_var: None,
            }),

            other => Err(self.err(format!("expected redirection, got `{other}`"))),
        }
    }
}

// ---------------------------------------------------------------------------
// Word decomposition
// ---------------------------------------------------------------------------

fn parse_dollar(
    chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>,
    nest: Nest<'_>,
) -> ParseResult<Option<Word>> {
    match chars.peek().map(|(_, c)| *c) {
        Some('(') => {
            if chars.clone().nth(1).map(|(_, c)| c) == Some('(') {
                let fragment = collect_balanced(chars, '(', ')')?;
                let expr = fragment
                    .strip_prefix("$((")
                    .and_then(|s| s.strip_suffix("))"))
                    .unwrap_or("")
                    .to_owned();
                return Ok(Some(Word::Arith(expr)));
            }
            let fragment = collect_balanced(chars, '(', ')')?;
            let inner = &fragment[2..fragment.len() - 1];
            let prog = Parser::new_at_depth(inner, nest.depth + 1, nest.aliases.cloned())
                .parse()
                .map_err(|e| ParseError {
                    msg: format!("in command substitution: {}", e.msg),
                    ..e
                })?;
            Ok(Some(Word::CmdSub(Box::new(prog.into_command()))))
        }
        Some('{') => {
            let fragment = collect_balanced(chars, '{', '}')?;
            Ok(Some(Word::Var(fragment[2..fragment.len() - 1].to_owned())))
        }
        _ => {
            let mut var = String::new();
            match chars.peek().map(|(_, c)| *c) {
                Some('@' | '*' | '#' | '?' | '-' | '$' | '!') => {
                    if let Some((_, c)) = chars.next() {
                        var.push(c);
                    }
                }
                Some(c) if c.is_ascii_digit() => {
                    if let Some((_, c)) = chars.next() {
                        var.push(c);
                    }
                }
                Some(c) if c.is_ascii_alphabetic() || c == '_' => {
                    while let Some(&(_, c)) = chars.peek() {
                        if c.is_ascii_alphanumeric() || c == '_' {
                            var.push(c);
                            chars.next();
                        } else {
                            break;
                        }
                    }
                }
                _ => return Ok(None),
            }
            Ok(Some(Word::Var(var)))
        }
    }
}

/// The word each reserved-word `Token` variant was lexed from: the text to
/// fall back to at a grammar position that expects a plain word (a
/// command's 2nd+ word, a case pattern, a `for` loop's variable, a case
/// subject, a redirection target) but got a reserved-word token instead.
///
/// The lexer recognizes reserved words unconditionally, with no notion of
/// where commands start and end, since it tokenizes one character at a
/// time with no lookback. That's *right* for the position that actually
/// matters, the first word of a new command (`if`/`while`/`done`/... need
/// to be reserved there, unconditionally, for the grammar to work at all),
/// but wrong everywhere else: `echo done` should print `done`, not fail to
/// parse, and `case $x in done) ...` needs `done` as a pattern. Each call
/// site above already knows from its own grammatical position that a
/// reserved-word token here can only mean the plain text it was lexed
/// from, so they fall back to it via this function instead of erroring.
const fn keyword_text(tok: &Token) -> Option<&'static str> {
    Some(match tok {
        Token::If => "if",
        Token::Then => "then",
        Token::Else => "else",
        Token::Elif => "elif",
        Token::Fi => "fi",
        Token::For => "for",
        Token::In => "in",
        Token::Do => "do",
        Token::Done => "done",
        Token::While => "while",
        Token::Until => "until",
        Token::Case => "case",
        Token::Esac => "esac",
        Token::Function => "function",
        // Not reserved words in the `if`/`while` sense, but subject to the
        // same positional rule: a standalone `{`/`}` anywhere but the start
        // of a command is just that character as text (`echo }` prints `}`).
        Token::LBrace => "{",
        Token::RBrace => "}",
        Token::Bang => "!",
        _ => return None,
    })
}

/// Splits a lexed word buffer into `Word` parts, resolving `$`/backtick
/// expansions and backslash escapes as it goes. `in_dquotes` selects which
/// escape rule the trailing `'\\'` arm applies: outside quotes, `\X` always
/// becomes a quoted `X` (any character), so it loses any special meaning
/// such as globbing or tilde expansion; inside double quotes,
/// POSIX narrows that to `\$ \` \" \\` and a trailing `\<newline>` (dropped
/// entirely); a backslash before anything else stays a literal backslash
/// followed by that character, e.g. `"\t"` is the two characters `\` and
/// `t`, not a tab.
fn decompose_word(raw: &str, nest: Nest<'_>, in_dquotes: bool) -> ParseResult<Vec<Word>> {
    // Backstop for the nesting this function reaches on its own (a quoted
    // region re-enters it, a `$(...)` re-enters the whole parser). The
    // sub-parsers below carry `depth` onward so the limit spans every hop,
    // not just the ones inside one `Parser`.
    if nest.depth > MAX_PARSE_DEPTH {
        return Err(ParseError {
            line: 0,
            col: 0,
            msg: "too deeply nested".to_owned(),
            incomplete: false,
            in_heredoc: false,
        });
    }
    let mut parts: Vec<Word> = Vec::new();
    let mut chars = raw.char_indices().peekable();
    let mut lit = String::new();

    macro_rules! flush_lit {
        () => {
            if !lit.is_empty() {
                parts.push(Word::Literal(std::mem::take(&mut lit)));
            }
        };
    }

    while let Some((_, ch)) = chars.next() {
        match ch {
            '$' => match parse_dollar(&mut chars, nest)? {
                Some(word) => {
                    flush_lit!();
                    parts.push(word);
                }
                None => lit.push('$'),
            },

            PROC_SUB => {
                flush_lit!();
                let output = chars.next().is_some_and(|(_, c)| c == '>');
                chars.next_if(|&(_, c)| c == '$');
                if let Some(Word::CmdSub(cmd)) = parse_dollar(&mut chars, nest)? {
                    parts.push(Word::ProcSub { cmd, output });
                }
            }

            QUOTE_START => {
                flush_lit!();
                let mut inner = String::new();
                for (_, c) in chars.by_ref() {
                    if c == QUOTE_END {
                        break;
                    }
                    inner.push(c);
                }
                parts.push(Word::Quoted(Box::new(Word::Literal(inner))));
            }

            '"' => {
                flush_lit!();
                parts.push(read_double_quoted(&mut chars, nest)?);
            }

            '`' => {
                flush_lit!();
                parts.push(read_backtick(&mut chars, nest)?);
            }

            '\\' if in_dquotes => match chars.peek().map(|&(_, c)| c) {
                Some('$' | '`' | '"' | '\\') => {
                    if let Some((_, c)) = chars.next() {
                        lit.push(c);
                    }
                }
                Some('\n') => {
                    chars.next();
                }
                _ => lit.push('\\'),
            },

            '\\' => {
                if let Some((_, next)) = chars.next() {
                    flush_lit!();
                    parts.push(Word::Quoted(Box::new(Word::Literal(next.to_string()))));
                }
            }

            other => lit.push(other),
        }
    }

    flush_lit!();

    if parts.is_empty() {
        parts.push(Word::Literal(String::new()));
    }

    Ok(parts)
}

/// Reads a `"..."` region, starting just past the opening quote, and returns
/// it as a single [`Word::Quoted`].
fn read_double_quoted(
    chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>,
    nest: Nest<'_>,
) -> ParseResult<Word> {
    let mut inner = String::new();
    while let Some((_, c)) = chars.next() {
        match c {
            '"' => break,
            // An escaped quote can't be the closing delimiter; consume the
            // pair atomically so the `"` isn't seen as anything but data
            // here (`decompose_word` resolves the escape on its own pass).
            '\\' => {
                inner.push('\\');
                if let Some((_, next)) = chars.next() {
                    inner.push(next);
                }
            }
            // A nested `$(...)` can itself contain quotes (e.g.
            // `"result: $(echo "nested")"`); skip the whole balanced span as
            // one unit so an inner `"` isn't mistaken for this string's
            // closing quote. The `decompose_word` call below re-parses it
            // properly once `inner` is correctly bounded.
            '$' if chars.peek().map(|&(_, c)| c) == Some('(') => {
                inner.push_str(&collect_balanced(chars, '(', ')')?);
            }
            '$' if chars.peek().map(|&(_, c)| c) == Some('{') => {
                inner.push_str(&collect_balanced(chars, '{', '}')?);
            }
            _ => inner.push(c),
        }
    }
    let mut sub_parts = decompose_word(&inner, nest.deeper(), true)?;
    let inner_word = if sub_parts.len() == 1 {
        sub_parts.remove(0)
    } else {
        Word::Compound(sub_parts)
    };
    Ok(Word::Quoted(Box::new(inner_word)))
}

/// Reads a `` `...` `` substitution, starting just past the opening backtick,
/// and returns it as a [`Word::CmdSub`].
fn read_backtick(
    chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>,
    nest: Nest<'_>,
) -> ParseResult<Word> {
    let mut inner = String::new();
    while let Some((_, c)) = chars.next() {
        match c {
            '`' => break,
            // POSIX: inside backquotes a backslash quotes only `$`, `` ` ``
            // and `\`, and is removed before them; an escaped backquote is
            // how one nests.
            '\\' => match chars.next() {
                Some((_, quoted @ ('$' | '`' | '\\'))) => inner.push(quoted),
                Some((_, other)) => {
                    inner.push('\\');
                    inner.push(other);
                }
                None => inner.push('\\'),
            },
            _ => inner.push(c),
        }
    }
    let program = Parser::new_at_depth(&inner, nest.depth + 1, nest.aliases.cloned()).parse()?;
    Ok(Word::CmdSub(Box::new(program.into_command())))
}

fn collect_balanced(
    chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>,
    open: char,
    close: char,
) -> ParseResult<String> {
    let mut buf = String::from('$');
    let mut depth = 0usize;

    for (_, c) in chars.by_ref() {
        buf.push(c);
        if c == open {
            depth += 1;
        } else if c == close {
            // Saturating, not `-= 1`: every current call site peeks the
            // opening delimiter before calling, so `depth` is at least 1
            // here, but that is a convention among callers rather than
            // anything this function enforces. An underflow would panic in
            // debug and wrap to `usize::MAX` in release.
            depth = depth.saturating_sub(1);
            if depth == 0 {
                break;
            }
        }
    }

    if depth != 0 {
        return Err(ParseError {
            line: 0,
            col: 0,
            msg: format!("unterminated `{open}...{close}` in word"),
            incomplete: true,
            in_heredoc: false,
        });
    }

    Ok(buf)
}

// ---------------------------------------------------------------------------
// Public convenience
// ---------------------------------------------------------------------------

/// Parses `raw`, already-lexed word text such as a `${var:-word}` operand,
/// into a [`Word`], with backslashes following double-quote rules when
/// `in_dquotes`.
///
/// # Errors
///
/// Returns an error if a quote or substitution in `raw` is left unclosed.
pub(crate) fn parse_word(
    raw: &str,
    in_dquotes: bool,
    aliases: Option<&Rc<Aliases>>,
) -> ParseResult<Word> {
    let marked;
    let raw = if in_dquotes || !raw.contains('\'') {
        raw
    } else {
        marked = mark_single_quotes(raw);
        &marked
    };
    let mut parts = decompose_word(raw, Nest { depth: 0, aliases }, in_dquotes)?;
    Ok(if parts.len() == 1 {
        parts.remove(0)
    } else {
        Word::Compound(parts)
    })
}

/// Replaces the `'` around each single-quoted region of `raw` with
/// [`QUOTE_START`]/[`QUOTE_END`], as the lexer does for ordinary words; text
/// inside `${...}` reaches the parser without that marking.
fn mark_single_quotes(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    let mut in_dquotes = false;
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                out.push(c);
                out.extend(chars.next());
            }
            '"' => {
                in_dquotes = !in_dquotes;
                out.push(c);
            }
            '\'' if !in_dquotes => {
                out.push(QUOTE_START);
                out.extend(chars.by_ref().take_while(|&c| c != '\''));
                out.push(QUOTE_END);
            }
            '$' if matches!(chars.peek(), Some('(' | '{')) => {
                out.push(c);
                let Some(open) = chars.next() else { break };
                let close = if open == '(' { ')' } else { '}' };
                out.push(open);
                let mut depth = 1usize;
                for c in chars.by_ref() {
                    out.push(c);
                    if c == open {
                        depth += 1;
                    } else if c == close {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                }
            }
            _ => out.push(c),
        }
    }
    out
}

/// # Errors
///
/// Returns an error if `src` fails to lex or parse.
pub fn parse(src: &str) -> ParseResult<Program> {
    Parser::new(src).parse()
}

/// [`parse`], expanding `aliases` while parsing, for input typed at the
/// prompt.
///
/// A here-document whose body runs into the end of `src`, wherever its
/// `<<` came from, makes the input incomplete (see
/// [`ParseError::in_heredoc`]) instead of ending there with a warning.
///
/// # Errors
///
/// Returns an error if `src` fails to lex or parse.
pub fn parse_with_aliases(src: &str, aliases: Rc<Aliases>) -> ParseResult<Program> {
    let mut parser = Parser::with_aliases(src, Some(aliases));
    parser.lexer.hold_unterminated_heredocs();
    parser.parse()
}

/// An alias's value, lexed into the tokens that replace the aliased word.
struct AliasTokens {
    tokens: Vec<Token>,
    /// Here-docs among `tokens` still waiting for a body, by index, with
    /// their delimiter and `<<-` flag.
    pending: Vec<(usize, (String, bool))>,
}

fn lex_alias_value(value: &str) -> ParseResult<AliasTokens> {
    let mut lexer = Lexer::new(value);
    let mut tokens = Vec::new();
    let mut unfilled = Vec::new();
    while !lexer.is_exhausted() {
        let token = lexer.next_token()?;
        if matches!(token, Token::HereDoc { .. }) {
            unfilled.push(tokens.len());
        }
        tokens.push(token);
        let bodies = lexer.take_heredoc_bodies();
        for (idx, body) in unfilled.drain(..bodies.len()).zip(bodies) {
            if let Token::HereDoc { body: slot, .. } = &mut tokens[idx] {
                *slot = body;
            }
        }
    }
    let pending = unfilled
        .into_iter()
        .zip(lexer.take_pending_heredocs())
        .collect();
    Ok(AliasTokens { tokens, pending })
}
