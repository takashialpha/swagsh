use std::fmt;

use crate::ast::{
    AndOrItem, AndOrList, AndOrOp, CaseArm, CaseClause, Command, ForClause, FunctionDef, GroupCmd,
    IfClause, Pipeline, Program, Redirect, RedirectKind, SimpleCmd, WhileClause, Word,
};
use crate::lexer::{LexError, RedirKind, RedirToken, Token};

// ---------------------------------------------------------------------------
// Parse errors
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ParseError {
    pub line: usize,
    pub col: usize,
    pub msg: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "parse error at {}:{} — {}",
            self.line, self.col, self.msg
        )
    }
}

impl std::error::Error for ParseError {}

impl From<LexError> for ParseError {
    fn from(e: LexError) -> Self {
        Self {
            line: e.line,
            col: e.col,
            msg: e.msg.to_owned(),
        }
    }
}

type ParseResult<T> = Result<T, ParseError>;

// ---------------------------------------------------------------------------
// Spanned token — the parser works on a pre-lexed flat list annotated with
// source positions for accurate error reporting.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Spanned {
    token: Token,
    line: usize,
    col: usize,
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

pub struct Parser {
    tokens: Vec<Spanned>,
    /// Cursor into `tokens`.
    pos: usize,
}

impl Parser {
    /// Build a parser from a raw source string, running the lexer internally.
    pub fn new(src: &str) -> ParseResult<Self> {
        use crate::lexer::Lexer;

        let mut lexer = Lexer::new(src);
        let mut tokens = Vec::new();

        loop {
            // We need position info; capture before advancing.
            let (line, col) = lexer.position();
            let tok = lexer.next_token()?;
            let done = tok == Token::Eof;
            tokens.push(Spanned {
                token: tok,
                line,
                col,
            });
            if done {
                break;
            }
        }

        Ok(Self { tokens, pos: 0 })
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

    /// Advance and return the consumed token.
    fn advance(&mut self) -> &Token {
        let t = &self.tokens[self.pos].token;
        if self.pos + 1 < self.tokens.len() {
            self.pos += 1;
        }
        t
    }

    /// Consume a token, returning an error if it doesn't match `expected`.
    fn expect(&mut self, expected: &Token) -> ParseResult<()> {
        if self.peek() == expected {
            self.advance();
            Ok(())
        } else {
            let sp = self.peek_spanned();
            Err(ParseError {
                line: sp.line,
                col: sp.col,
                msg: format!("expected `{expected}`, got `{}`", sp.token),
            })
        }
    }

    fn err(&self, msg: impl Into<String>) -> ParseError {
        let sp = self.peek_spanned();
        ParseError {
            line: sp.line,
            col: sp.col,
            msg: msg.into(),
        }
    }

    /// Skip zero or more newlines and semicolons (used as separator).
    fn skip_newlines(&mut self) {
        while matches!(self.peek(), Token::Newline | Token::Semi) {
            self.advance();
        }
    }

    /// Return `true` and consume if the next token matches.
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

    pub fn parse(mut self) -> ParseResult<Program> {
        let body = self.parse_list()?;
        if *self.peek() != Token::Eof {
            return Err(self.err(format!("unexpected token `{}`", self.peek())));
        }
        Ok(Program { body })
    }

    // ------------------------------------------------------------------
    // List  ::=  (NewlineList | AndOrList (';' | '&' | '\n')*)*
    // ------------------------------------------------------------------

    /// Parse a sequence of and-or lists separated by `;`, `&`, or newlines.
    /// Stops at `Eof`, `fi`, `done`, `esac`, `elif`, `else`, `then`, `)`
    /// or `}` — all of which are consumed by the caller.
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
                | Token::RParen
                | Token::RBrace
                | Token::In
        )
    }

    // ------------------------------------------------------------------
    // AndOrList  ::=  Pipeline (('&&' | '||') Newlines Pipeline)* ('&'|';')?
    // ------------------------------------------------------------------

    fn parse_and_or(&mut self) -> ParseResult<AndOrList> {
        let mut items = Vec::new();
        let pipeline = self.parse_pipeline()?;

        let first_op = self.peek_and_or_op();
        items.push(AndOrItem {
            command: pipeline,
            op: first_op,
        });

        if first_op.is_some() {
            self.advance(); // consume && / ||
            self.skip_newlines();
            // parse remaining items
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

        // trailing & or ; — determines async and consumes separator
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
        let negated = self.eat(&Token::Bang);
        let mut commands = Vec::new();

        commands.push(self.parse_command()?);

        while self.eat(&Token::Pipe) {
            self.skip_newlines();
            commands.push(self.parse_command()?);
        }

        Ok(Pipeline { commands, negated })
    }

    // ------------------------------------------------------------------
    // Command  ::=  CompoundCommand | FunctionDef | SimpleCommand
    // ------------------------------------------------------------------

    fn parse_command(&mut self) -> ParseResult<Command> {
        match self.peek() {
            Token::If => self.parse_if(),
            Token::For => self.parse_for(),
            Token::While => self.parse_while(false),
            Token::Until => self.parse_while(true),
            Token::Case => self.parse_case(),
            Token::LBrace => self.parse_brace_group(),
            Token::LParen => self.parse_subshell(),
            Token::Function => self.parse_function_def(),
            Token::Word(_) if self.is_function_def_ahead() => self.parse_function_def(),
            _ => self.parse_simple_cmd().map(Command::Simple),
        }
    }

    /// Peek ahead to check for `name ()` function definition syntax.
    fn is_function_def_ahead(&self) -> bool {
        // current token is Word, next non-whitespace should be `(`
        if let Token::Word(_) = self.peek()
            && self.pos + 1 < self.tokens.len() {
                return matches!(self.tokens[self.pos + 1].token, Token::LParen);
            }
        false
    }

    // ------------------------------------------------------------------
    // SimpleCommand
    // ------------------------------------------------------------------

    fn parse_simple_cmd(&mut self) -> ParseResult<SimpleCmd> {
        let mut words = Vec::new();
        let mut redirects = Vec::new();

        loop {
            match self.peek() {
                Token::Word(_) => {
                    if let Token::Word(w) = self.advance().clone() {
                        words.push(self.parse_word_str(&w)?);
                    }
                }
                Token::Redir(_) => {
                    redirects.push(self.parse_redirect()?);
                }
                Token::HereString(_) | Token::HereDoc { .. } => {
                    redirects.push(self.parse_redirect()?);
                }
                _ => break,
            }
        }

        if words.is_empty() && redirects.is_empty() {
            return Err(self.err("expected a command"));
        }

        Ok(SimpleCmd { words, redirects })
    }

    // ------------------------------------------------------------------
    // Word parsing — convert a raw token string into a `Word` node
    // ------------------------------------------------------------------

    fn parse_word_str(&self, raw: &str) -> ParseResult<Word> {
        let bytes = raw.as_bytes();

        // Fast path — pure literal (no `$`, `` ` ``, `"`, `\`).
        if !bytes
            .iter()
            .any(|&b| matches!(b, b'$' | b'`' | b'"' | b'\\'))
        {
            return Ok(Word::Literal(raw.to_owned()));
        }

        // Check for bare arithmetic expansion `$(( ))` — error per spec.
        if raw.starts_with("$((") {
            let sp = self.peek_spanned();
            return Err(ParseError {
                line: sp.line,
                col: sp.col,
                msg: "arithmetic expansion `$(( ))` is not supported".to_owned(),
            });
        }

        // Compound word — decompose into parts.
        let parts = decompose_word(raw, self)?;
        if parts.len() == 1 {
            Ok(parts.into_iter().next().unwrap())
        } else {
            Ok(Word::Compound(parts))
        }
    }

    // ------------------------------------------------------------------
    // Redirect
    // ------------------------------------------------------------------

    fn parse_redirect(&mut self) -> ParseResult<Redirect> {
        match self.advance().clone() {
            Token::Redir(RedirToken { kind, fd }) => {
                let target_tok = self.peek().clone();
                let target = match target_tok {
                    Token::Word(ref w) => {
                        let word = self.parse_word_str(w)?;
                        self.advance();
                        word
                    }
                    _ => return Err(self.err("expected filename after redirection")),
                };

                let (rk, default_fd) = match kind {
                    RedirKind::Out => (RedirectKind::Out, 1),
                    RedirKind::Append => (RedirectKind::Append, 1),
                    RedirKind::In => (RedirectKind::In, 0),
                    RedirKind::InOut => (RedirectKind::In, 0), // treat <> as <
                    RedirKind::OutFd => (RedirectKind::FdOut, 1),
                    RedirKind::BothOut => (RedirectKind::Both, 1),
                    RedirKind::BothAppend => (RedirectKind::Append, 1),
                };

                Ok(Redirect {
                    kind: rk,
                    fd: fd.map(|n| n as i32).unwrap_or(default_fd),
                    target,
                })
            }

            Token::HereString(s) => Ok(Redirect {
                kind: RedirectKind::HereString,
                fd: 0,
                target: Word::Literal(s),
            }),

            Token::HereDoc { delimiter, .. } => {
                // Body is collected by the executor at runtime (deferred).
                Ok(Redirect {
                    kind: RedirectKind::In,
                    fd: 0,
                    target: Word::Literal(format!("<<{delimiter}")),
                })
            }

            other => Err(self.err(format!("expected redirection, got `{other}`"))),
        }
    }

    // ------------------------------------------------------------------
    // Compound commands
    // ------------------------------------------------------------------

    // if condition; then body (elif condition; then body)* (else body)? fi
    fn parse_if(&mut self) -> ParseResult<Command> {
        self.expect(&Token::If)?;
        let condition = self.parse_list()?;
        self.expect(&Token::Then)?;
        let then_body = self.parse_list()?;

        let mut elif_clauses = Vec::new();
        let mut else_body = None;

        loop {
            match self.peek() {
                Token::Elif => {
                    self.advance();
                    let elif_cond = self.parse_list()?;
                    self.expect(&Token::Then)?;
                    let elif_body = self.parse_list()?;
                    elif_clauses.push((elif_cond, elif_body));
                }
                Token::Else => {
                    self.advance();
                    else_body = Some(self.parse_list()?);
                    break;
                }
                _ => break,
            }
        }

        self.expect(&Token::Fi)?;

        Ok(Command::If(IfClause {
            condition,
            then_body,
            elif_clauses,
            else_body,
        }))
    }

    // for var in words; do body; done
    fn parse_for(&mut self) -> ParseResult<Command> {
        self.expect(&Token::For)?;

        let var = match self.advance().clone() {
            Token::Word(name) => name,
            other => {
                return Err(self.err(format!("expected variable name after `for`, got `{other}`")));
            }
        };

        self.skip_newlines();

        // Optional `in wordlist` — if absent defaults to "$@"
        let items = if self.eat(&Token::In) {
            let mut words = Vec::new();
            while let Token::Word(w) = self.peek().clone() {
                let word = self.parse_word_str(&w)?;
                self.advance();
                words.push(word);
            }
            words
        } else {
            vec![Word::Var("@".to_owned())]
        };

        self.skip_newlines();
        self.eat(&Token::Semi);
        self.skip_newlines();
        self.expect(&Token::Do)?;
        let body = self.parse_list()?;
        self.expect(&Token::Done)?;

        Ok(Command::For(ForClause { var, items, body }))
    }

    // while/until condition; do body; done
    fn parse_while(&mut self, until: bool) -> ParseResult<Command> {
        // consume `while` or `until`
        self.advance();

        let condition = self.parse_list()?;
        self.expect(&Token::Do)?;
        let body = self.parse_list()?;
        self.expect(&Token::Done)?;

        Ok(Command::While(WhileClause {
            condition,
            body,
            until,
        }))
    }

    // case word in (pattern | ...) ) body ;; ... esac
    fn parse_case(&mut self) -> ParseResult<Command> {
        self.expect(&Token::Case)?;

        let word_raw = match self.advance().clone() {
            Token::Word(w) => w,
            other => return Err(self.err(format!("expected word after `case`, got `{other}`"))),
        };
        let word = self.parse_word_str(&word_raw)?;

        self.skip_newlines();
        self.expect(&Token::In)?;
        self.skip_newlines();

        let mut arms = Vec::new();

        loop {
            if self.eat(&Token::Esac) {
                break;
            }

            // optional leading `(`
            self.eat(&Token::LParen);

            // pattern list separated by `|`
            let mut patterns = Vec::new();
            loop {
                match self.advance().clone() {
                    Token::Word(p) => patterns.push(self.parse_word_str(&p)?),
                    other => {
                        return Err(self.err(format!("expected pattern in case arm, got `{other}`")));
                    }
                }
                if !self.eat(&Token::Pipe) {
                    break;
                }
            }

            self.expect(&Token::RParen)?;
            self.skip_newlines();

            let body = self.parse_list()?;

            // `;;` terminates the arm — optional before `esac`
            self.eat(&Token::SemiSemi);
            self.skip_newlines();

            arms.push(CaseArm { patterns, body });
        }

        Ok(Command::Case(CaseClause { word, arms }))
    }

    // { list; }
    fn parse_brace_group(&mut self) -> ParseResult<Command> {
        self.expect(&Token::LBrace)?;
        self.skip_newlines();
        let body = self.parse_list()?;
        self.expect(&Token::RBrace)?;
        Ok(Command::Group(GroupCmd {
            body,
            subshell: false,
        }))
    }

    // ( list )
    fn parse_subshell(&mut self) -> ParseResult<Command> {
        self.expect(&Token::LParen)?;
        self.skip_newlines();
        let body = self.parse_list()?;
        self.expect(&Token::RParen)?;
        Ok(Command::Group(GroupCmd {
            body,
            subshell: true,
        }))
    }

    // function name() { body; }  or  name() { body; }
    fn parse_function_def(&mut self) -> ParseResult<Command> {
        // consume optional `function` keyword
        let has_keyword = self.eat(&Token::Function);

        let name = match self.advance().clone() {
            Token::Word(n) => n,
            other => return Err(self.err(format!("expected function name, got `{other}`"))),
        };

        if !has_keyword {
            // name () form — must have `()`
            self.expect(&Token::LParen)?;
            self.expect(&Token::RParen)?;
        } else if self.eat(&Token::LParen) {
            // `function name()` form — parens are optional but consume if present
            self.expect(&Token::RParen)?;
        }

        self.skip_newlines();

        let body = self.parse_command()?;

        Ok(Command::FunctionDef(FunctionDef {
            name,
            body: Box::new(body),
        }))
    }
}

// ---------------------------------------------------------------------------
// Word decomposition
// Converts a raw lexer string (which may contain `$VAR`, `$(cmd)`, `"..."`)
// into a Vec<Word> that the executor can expand piece by piece.
// ---------------------------------------------------------------------------

fn decompose_word(raw: &str, parser: &Parser) -> ParseResult<Vec<Word>> {
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
            '$' => {
                match chars.peek().map(|(_, c)| *c) {
                    Some('(') => {
                        // Reject arithmetic expansion before collecting.
                        if chars.clone().nth(1).map(|(_, c)| c) == Some('(') {
                            return Err(
                                parser.err("arithmetic expansion `$(( ))` is not supported")
                            );
                        }
                        // command substitution $(...)
                        flush_lit!();
                        let start = raw.find("$(").unwrap_or(0);
                        let fragment = collect_balanced(raw, &mut chars, '(', ')')?;
                        let inner = &fragment[2..fragment.len() - 1];
                        let sub_parser = Parser::new(inner).map_err(|e| ParseError {
                            line: e.line,
                            col: e.col,
                            msg: format!("inside command substitution at col {start}: {}", e.msg),
                        })?;
                        let program = sub_parser.parse().map_err(|e| ParseError {
                            line: e.line,
                            col: e.col,
                            msg: format!("inside command substitution: {}", e.msg),
                        })?;
                        let cmd = if program.body.len() == 1
                            && program.body[0].items.len() == 1
                            && !program.body[0].is_async
                        {
                            let pl = &program.body[0].items[0].command;
                            if pl.commands.len() == 1 {
                                pl.commands[0].clone()
                            } else {
                                Command::Pipeline(pl.clone())
                            }
                        } else {
                            Command::Group(crate::ast::GroupCmd {
                                body: program.body,
                                subshell: true,
                            })
                        };
                        parts.push(Word::CmdSub(Box::new(cmd)));
                    }
                    Some('{') => {
                        // ${VAR} or ${VAR:-default} etc.
                        flush_lit!();
                        let fragment = collect_balanced(raw, &mut chars, '{', '}')?;
                        let var_expr = &fragment[2..fragment.len() - 1];
                        parts.push(Word::Var(var_expr.to_owned()));
                    }
                    _ => {
                        // bare $VAR or $@, $*, $?, $#, $-, $$, $!, $0-$9
                        flush_lit!();
                        let mut var = String::new();
                        match chars.peek().map(|(_, c)| *c) {
                            Some('@' | '*' | '#' | '?' | '-' | '$' | '!') => {
                                var.push(chars.next().unwrap().1);
                            }
                            Some(c) if c.is_ascii_digit() => {
                                var.push(chars.next().unwrap().1);
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
                            _ => {
                                // lone `$` — treat as literal
                                lit.push('$');
                                continue;
                            }
                        }
                        parts.push(Word::Var(var));
                    }
                }
            }

            '"' => {
                // double-quoted region — recurse on the inner content
                flush_lit!();
                let mut inner = String::new();
                for (_, c) in chars.by_ref() {
                    if c == '"' {
                        break;
                    }
                    inner.push(c);
                }
                // inner may itself contain $VAR / $(cmd)
                if inner.chars().any(|c| c == '$' || c == '`') {
                    let sub_parts = decompose_word(&inner, parser)?;
                    parts.extend(sub_parts);
                } else {
                    parts.push(Word::Literal(inner));
                }
            }

            '`' => {
                // backtick substitution — collect until closing backtick
                flush_lit!();
                let mut inner = String::new();
                for (_, c) in chars.by_ref() {
                    if c == '`' {
                        break;
                    }
                    inner.push(c);
                }
                let sub_parser = Parser::new(&inner)?;
                let program = sub_parser.parse()?;
                let cmd = if program.body.len() == 1 && program.body[0].items.len() == 1 {
                    let pl = &program.body[0].items[0].command;
                    if pl.commands.len() == 1 {
                        pl.commands[0].clone()
                    } else {
                        Command::Pipeline(pl.clone())
                    }
                } else {
                    Command::Group(crate::ast::GroupCmd {
                        body: program.body,
                        subshell: true,
                    })
                };
                parts.push(Word::CmdSub(Box::new(cmd)));
            }

            '\\' => {
                // escape — consume next character literally
                if let Some((_, next)) = chars.next() {
                    lit.push(next);
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

/// Collect a balanced `open`/`close` delimited region starting from the
/// `open` already peeked (the `$` was already consumed by the caller).
/// Returns the full fragment including the leading `$` + `open` and trailing `close`.
fn collect_balanced(
    _raw: &str,
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
            depth -= 1;
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
        });
    }

    Ok(buf)
}

// ---------------------------------------------------------------------------
// Public convenience
// ---------------------------------------------------------------------------

/// Parse a complete source string into a `Program`.
pub fn parse(src: &str) -> ParseResult<Program> {
    Parser::new(src)?.parse()
}
