use std::fmt;

use crate::ast::{
    AndOrItem, AndOrList, AndOrOp, Command, Pipeline, Program, Redirect, RedirectKind, SimpleCmd,
    Word,
};
use crate::lexer::{
    LexError, QUOTE_END, QUOTE_END_BYTE, QUOTE_START, QUOTE_START_BYTE, RedirKind, RedirToken,
    Token,
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
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

pub struct Parser {
    tokens: Vec<Spanned>,
    pos: usize,
}

impl Parser {
    pub fn new(src: &str) -> ParseResult<Self> {
        use crate::lexer::Lexer;

        let mut lexer = Lexer::new(src);
        let mut tokens = Vec::new();

        loop {
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

    fn advance(&mut self) -> &Token {
        let t = &self.tokens[self.pos].token;
        if self.pos + 1 < self.tokens.len() {
            self.pos += 1;
        }
        t
    }

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
                incomplete: sp.token == Token::Eof,
            })
        }
    }

    /// The token the error points at ran out (`Token::Eof`): the grammar
    /// wanted more but the input simply ended, e.g. `if true` with no
    /// `then` yet. Every hard syntax error instead points at a real,
    /// unexpected token, so this check alone is enough to tell the two
    /// apart everywhere `err`/`expect` are used.
    fn err(&self, msg: impl Into<String>) -> ParseError {
        let sp = self.peek_spanned();
        ParseError {
            line: sp.line,
            col: sp.col,
            msg: msg.into(),
            incomplete: sp.token == Token::Eof,
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
        let mut words = Vec::new();
        let mut redirects = Vec::new();

        loop {
            match self.peek() {
                Token::Word(_) => {
                    if let Token::Word(w) = self.advance().clone() {
                        words.push(self.parse_word_str(&w)?);
                    }
                }
                Token::Redir(_) | Token::HereString(_) | Token::HereDoc { .. } => {
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
    // Word parsing
    // ------------------------------------------------------------------

    fn parse_word_str(&self, raw: &str) -> ParseResult<Word> {
        let bytes = raw.as_bytes();

        if !bytes.iter().any(|&b| {
            matches!(
                b,
                b'$' | b'`' | b'"' | b'\\' | QUOTE_START_BYTE | QUOTE_END_BYTE
            )
        }) {
            return Ok(Word::Literal(raw.to_owned()));
        }

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
                    RedirKind::OutFd => (RedirectKind::FdOut, 1),
                    RedirKind::BothOut => (RedirectKind::Both, 1),
                };

                Ok(Redirect {
                    kind: rk,
                    fd: fd.map_or(default_fd, u32::cast_signed),
                    target,
                })
            }

            Token::HereDoc { body, quoted } => Ok(Redirect {
                kind: RedirectKind::HereDoc {
                    raw_body: body,
                    quoted,
                },
                fd: 0,
                target: Word::Literal(String::new()),
            }),

            Token::HereString(s) => Ok(Redirect {
                kind: RedirectKind::HereString,
                fd: 0,
                target: Word::Literal(s),
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
    _parser: &Parser,
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
            let prog = Parser::new(inner)
                .and_then(Parser::parse)
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
                _ => return Ok(None),
            }
            Ok(Some(Word::Var(var)))
        }
    }
}

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
            '$' => match parse_dollar(&mut chars, parser)? {
                Some(word) => {
                    flush_lit!();
                    parts.push(word);
                }
                None => lit.push('$'),
            },

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
                let mut inner = String::new();
                for (_, c) in chars.by_ref() {
                    if c == '"' {
                        break;
                    }
                    inner.push(c);
                }
                let inner_word = if inner.chars().any(|c| c == '$' || c == '`') {
                    let sub_parts = decompose_word(&inner, parser)?;
                    if sub_parts.len() == 1 {
                        sub_parts.into_iter().next().unwrap()
                    } else {
                        Word::Compound(sub_parts)
                    }
                } else {
                    Word::Literal(inner)
                };
                parts.push(Word::Quoted(Box::new(inner_word)));
            }

            '`' => {
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
                parts.push(Word::CmdSub(Box::new(program.into_command())));
            }

            '\\' => {
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
            incomplete: true,
        });
    }

    Ok(buf)
}

// ---------------------------------------------------------------------------
// Public convenience
// ---------------------------------------------------------------------------

pub fn parse(src: &str) -> ParseResult<Program> {
    Parser::new(src)?.parse()
}
