use std::fmt;

// UTF-8 byte classification used by the lexer to handle multi-byte characters.
// Any byte >= 0x80 is part of a multi-byte sequence: start bytes are 0xC0-0xFF,
// continuation bytes are 0x80-0xBF. ASCII is 0x00-0x7F (< 0x80).
const UTF8_CONT_START: u8 = 0x80; // first continuation byte
const UTF8_CONT_END: u8 = 0xBF; // last continuation byte

// Sentinels used to communicate single-quote boundaries to decompose_word.
// These control characters (SOH/STX) cannot appear in valid shell source.
pub const QUOTE_START: char = '\x01';
pub const QUOTE_END: char = '\x02';
pub const QUOTE_START_BYTE: u8 = QUOTE_START as u8;
pub const QUOTE_END_BYTE: u8 = QUOTE_END as u8;

// ---------------------------------------------------------------------------
// Token types
// ---------------------------------------------------------------------------

/// Every syntactic atom the shell lexer can emit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    // Words & literals
    Word(String),
    // Operators
    Pipe,
    OrOr,
    Ampersand,
    AndAnd,
    Semi,
    SemiSemi,
    LParen,
    RParen,
    LBrace,
    RBrace,
    Bang,
    // Redirections
    Redir(RedirToken),
    // Here-documents & here-strings
    HereDoc { body: String, quoted: bool },
    HereString(String),
    // Reserved words
    If,
    Then,
    Else,
    Elif,
    Fi,
    For,
    In,
    Do,
    Done,
    While,
    Until,
    Case,
    Esac,
    Function,
    // Structural
    Newline,
    Eof,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedirToken {
    pub kind: RedirKind,
    pub fd: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedirKind {
    Out,
    Append,
    In,
    OutFd,
    BothOut,
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Word(w) => write!(f, "Word({w:?})"),
            Self::Pipe => f.write_str("|"),
            Self::OrOr => f.write_str("||"),
            Self::Ampersand => f.write_str("&"),
            Self::AndAnd => f.write_str("&&"),
            Self::Semi => f.write_str(";"),
            Self::SemiSemi => f.write_str(";;"),
            Self::LParen => f.write_str("("),
            Self::RParen => f.write_str(")"),
            Self::LBrace => f.write_str("{"),
            Self::RBrace => f.write_str("}"),
            Self::Bang => f.write_str("!"),
            Self::Newline => f.write_str("\\n"),
            Self::Eof => f.write_str("<EOF>"),
            Self::HereDoc { .. } => f.write_str("here-doc"),
            _ => write!(f, "{self:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Lexer errors
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexError {
    pub line: usize,
    pub col: usize,
    pub msg: &'static str,
}

impl fmt::Display for LexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "lex error at {}:{}: {}", self.line, self.col, self.msg)
    }
}

impl std::error::Error for LexError {}

// ---------------------------------------------------------------------------
// Lexer state machine
// ---------------------------------------------------------------------------

pub struct Lexer<'src> {
    src: &'src [u8],
    pos: usize,
    line: usize,
    col: usize,
}

impl<'src> Lexer<'src> {
    pub const fn new(src: &'src str) -> Self {
        Self {
            src: src.as_bytes(),
            pos: 0,
            line: 1,
            col: 1,
        }
    }

    /// Current source position: called by the parser before each token advance
    /// to attach accurate span information.
    #[inline]
    pub const fn position(&self) -> (usize, usize) {
        (self.line, self.col)
    }

    // ------------------------------------------------------------------
    // Cursor helpers
    // ------------------------------------------------------------------

    #[inline]
    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    #[inline]
    fn peek2(&self) -> Option<u8> {
        self.src.get(self.pos + 1).copied()
    }

    #[inline]
    fn advance(&mut self) -> Option<u8> {
        let b = self.src.get(self.pos).copied()?;
        self.pos += 1;
        if b == b'\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        Some(b)
    }

    #[inline]
    const fn err(&self, msg: &'static str) -> LexError {
        LexError {
            line: self.line,
            col: self.col,
            msg,
        }
    }

    /// Push a complete UTF-8 scalar to `buf`.
    ///
    /// Call this after `advance()` has consumed the first byte of the sequence
    /// and returned it. `start` must be `self.pos - 1` (the byte just consumed).
    /// Continuation bytes (0x80-0xBF) are consumed and the full sequence is
    /// appended to `buf` as a valid UTF-8 str slice.
    ///
    /// SAFETY: `self.src` is from a valid `&str`, so any run of bytes starting
    /// with a non-continuation byte and followed only by continuation bytes is
    /// a complete, valid UTF-8 scalar. Slicing at these positions is safe.
    #[inline]
    fn push_utf8(&mut self, buf: &mut String, start: usize) {
        while matches!(self.peek(), Some(UTF8_CONT_START..=UTF8_CONT_END)) {
            self.advance();
        }
        buf.push_str(unsafe { std::str::from_utf8_unchecked(&self.src[start..self.pos]) });
    }

    // ------------------------------------------------------------------
    // Whitespace & comments
    // ------------------------------------------------------------------

    fn skip_whitespace(&mut self) {
        while let Some(b) = self.peek() {
            match b {
                b' ' | b'\t' | b'\r' => {
                    self.advance();
                }
                b'\\' if self.peek2() == Some(b'\n') => {
                    self.advance();
                    self.advance();
                }
                _ => break,
            }
        }
    }

    fn skip_comment(&mut self) {
        while let Some(b) = self.peek() {
            if b == b'\n' {
                break;
            }
            self.advance();
        }
    }

    // ------------------------------------------------------------------
    // Quoted regions
    // ------------------------------------------------------------------

    fn lex_single_quoted(&mut self, buf: &mut String) -> Result<(), LexError> {
        loop {
            match self.advance() {
                None => return Err(self.err("unterminated single-quoted string")),
                Some(b'\'') => break,
                Some(b) if b < UTF8_CONT_START => buf.push(b as char),
                Some(_) => self.push_utf8(buf, self.pos - 1),
            }
        }
        Ok(())
    }

    fn lex_double_quoted(&mut self, buf: &mut String) -> Result<(), LexError> {
        buf.push('"');
        loop {
            match self.advance() {
                None => return Err(self.err("unterminated double-quoted string")),
                Some(b'"') => {
                    buf.push('"');
                    break;
                }
                Some(b'\\') => match self.peek() {
                    Some(b @ (b'$' | b'`' | b'"' | b'\\' | b'\n')) => {
                        self.advance();
                        if b != b'\n' {
                            buf.push('\\');
                            buf.push(b as char);
                        }
                    }
                    _ => {
                        buf.push('\\');
                    }
                },
                Some(b) if b < UTF8_CONT_START => buf.push(b as char),
                Some(_) => self.push_utf8(buf, self.pos - 1),
            }
        }
        Ok(())
    }

    fn lex_cmd_sub(&mut self, buf: &mut String) -> Result<(), LexError> {
        buf.push_str("$(");
        let mut depth: usize = 1;
        loop {
            match self.advance() {
                None => return Err(self.err("unterminated command substitution")),
                Some(b'(') => {
                    depth += 1;
                    buf.push('(');
                }
                Some(b')') => {
                    depth -= 1;
                    if depth == 0 {
                        buf.push(')');
                        break;
                    }
                    buf.push(')');
                }
                Some(b'\'') => {
                    buf.push('\'');
                    self.lex_single_quoted(buf)?;
                    buf.push('\'');
                }
                Some(b'"') => self.lex_double_quoted(buf)?,
                Some(b'\\') => {
                    buf.push('\\');
                    if let Some(n) = self.advance() {
                        if n < UTF8_CONT_START {
                            buf.push(n as char);
                        } else {
                            self.push_utf8(buf, self.pos - 1);
                        }
                    }
                }
                Some(b) if b < UTF8_CONT_START => buf.push(b as char),
                Some(_) => self.push_utf8(buf, self.pos - 1),
            }
        }
        Ok(())
    }

    fn lex_param_expand(&mut self, buf: &mut String) -> Result<(), LexError> {
        buf.push_str("${");
        let mut depth: usize = 1;
        loop {
            match self.advance() {
                None => return Err(self.err("unterminated parameter expansion")),
                Some(b'{') => {
                    depth += 1;
                    buf.push('{');
                }
                Some(b'}') => {
                    depth -= 1;
                    buf.push('}');
                    if depth == 0 {
                        break;
                    }
                }
                Some(b) if b < UTF8_CONT_START => buf.push(b as char),
                Some(_) => self.push_utf8(buf, self.pos - 1),
            }
        }
        Ok(())
    }

    fn lex_backtick(&mut self, buf: &mut String) -> Result<(), LexError> {
        buf.push('`');
        loop {
            match self.advance() {
                None => return Err(self.err("unterminated backtick substitution")),
                Some(b'`') => {
                    buf.push('`');
                    break;
                }
                Some(b'\\') => {
                    buf.push('\\');
                    if let Some(n) = self.advance() {
                        if n < UTF8_CONT_START {
                            buf.push(n as char);
                        } else {
                            self.push_utf8(buf, self.pos - 1);
                        }
                    }
                }
                Some(b) if b < UTF8_CONT_START => buf.push(b as char),
                Some(_) => self.push_utf8(buf, self.pos - 1),
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Here-document
    // ------------------------------------------------------------------

    fn lex_heredoc(&mut self, strip_tabs: bool) -> Result<Token, LexError> {
        self.skip_whitespace();
        let mut delim = String::new();
        let mut quoted = false;
        loop {
            match self.peek() {
                None | Some(b'\n' | b' ' | b'\t') => break,
                Some(b'\'') => {
                    self.advance();
                    self.lex_single_quoted(&mut delim)?;
                    quoted = true;
                }
                Some(b'"') => {
                    self.advance();
                    let start = delim.len();
                    self.lex_double_quoted(&mut delim)?;
                    delim.remove(start);
                    if delim.ends_with('"') {
                        delim.pop();
                    }
                }
                Some(b) => {
                    self.advance();
                    delim.push(b as char);
                }
            }
        }
        if delim.is_empty() {
            return Err(self.err("empty here-doc delimiter"));
        }

        // Consume the remainder of the current line up to and including the newline.
        while let Some(b) = self.peek() {
            if b == b'\n' {
                self.advance();
                break;
            }
            self.advance();
        }

        // Collect body lines until a line equal to the delimiter is found.
        let mut body = String::new();
        loop {
            let mut line = String::new();
            loop {
                match self.peek() {
                    None => return Ok(Token::HereDoc { body, quoted }),
                    Some(b'\n') => {
                        self.advance();
                        break;
                    }
                    Some(b) if b < UTF8_CONT_START => {
                        line.push(b as char);
                        self.advance();
                    }
                    Some(_) => {
                        let start = self.pos;
                        self.advance();
                        self.push_utf8(&mut line, start);
                    }
                }
            }
            let check = if strip_tabs {
                line.trim_start_matches('\t')
            } else {
                line.as_str()
            };
            if check == delim {
                break;
            }
            body.push_str(check);
            body.push('\n');
        }

        Ok(Token::HereDoc { body, quoted })
    }

    // ------------------------------------------------------------------
    // Redirection
    // ------------------------------------------------------------------

    fn lex_redir(&mut self, fd: Option<u32>) -> Result<Token, LexError> {
        let first = self.advance().unwrap();
        let kind = match first {
            b'>' => {
                if self.peek() == Some(b'>') {
                    self.advance();
                    RedirKind::Append
                } else if self.peek() == Some(b'&') {
                    self.advance();
                    RedirKind::OutFd
                } else {
                    RedirKind::Out
                }
            }
            b'<' => match self.peek() {
                Some(b'<') => {
                    self.advance();
                    if self.peek() == Some(b'<') {
                        self.advance();
                        self.skip_whitespace();
                        let mut s = String::new();
                        self.lex_word_into(&mut s)?;
                        return Ok(Token::HereString(s));
                    } else if self.peek() == Some(b'-') {
                        self.advance();
                        return self.lex_heredoc(true);
                    }
                    return self.lex_heredoc(false);
                }
                _ => RedirKind::In,
            },
            b'&' => {
                if self.peek() == Some(b'>') {
                    self.advance();
                    RedirKind::BothOut
                } else {
                    unreachable!("lex_redir called for plain &");
                }
            }
            _ => unreachable!(),
        };
        Ok(Token::Redir(RedirToken { kind, fd }))
    }

    // ------------------------------------------------------------------
    // Word accumulation
    // ------------------------------------------------------------------

    pub fn lex_word_into(&mut self, buf: &mut String) -> Result<(), LexError> {
        loop {
            match self.peek() {
                None
                | Some(
                    b' ' | b'\t' | b'\r' | b'\n' | b';' | b'|' | b'&' | b'(' | b')' | b'{' | b'}'
                    | b'<' | b'>',
                ) => break,
                Some(b'#') => {
                    if buf.is_empty() {
                        break;
                    }
                    buf.push('#');
                    self.advance();
                }
                Some(b'\\') => {
                    self.advance();
                    match self.advance() {
                        None => break,
                        Some(b'\n') => {}
                        Some(b) => {
                            buf.push('\\');
                            buf.push(b as char);
                        }
                    }
                }
                Some(b'\'') => {
                    self.advance();
                    buf.push(QUOTE_START);
                    self.lex_single_quoted(buf)?;
                    buf.push(QUOTE_END);
                }
                Some(b'"') => {
                    self.advance();
                    self.lex_double_quoted(buf)?;
                }
                Some(b'$') => {
                    self.advance();
                    match self.peek() {
                        Some(b'(') => {
                            self.advance();
                            self.lex_cmd_sub(buf)?;
                        }
                        Some(b'{') => {
                            self.advance();
                            self.lex_param_expand(buf)?;
                        }
                        _ => {
                            buf.push('$');
                            match self.peek() {
                                Some(
                                    b'@' | b'*' | b'#' | b'?' | b'-' | b'$' | b'!' | b'0'..=b'9',
                                ) => {
                                    buf.push(self.advance().unwrap() as char);
                                }
                                Some(b'a'..=b'z' | b'A'..=b'Z' | b'_') => {
                                    while matches!(
                                        self.peek(),
                                        Some(b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_')
                                    ) {
                                        buf.push(self.advance().unwrap() as char);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
                Some(b'`') => {
                    self.advance();
                    self.lex_backtick(buf)?;
                }
                Some(b) if b < UTF8_CONT_START => {
                    buf.push(b as char);
                    self.advance();
                }
                Some(_) => {
                    let start = self.pos;
                    self.advance();
                    self.push_utf8(buf, start);
                }
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Public interface
    // ------------------------------------------------------------------

    pub fn next_token(&mut self) -> Result<Token, LexError> {
        self.skip_whitespace();

        match self.peek() {
            None => return Ok(Token::Eof),
            Some(b'\n') => {
                self.advance();
                return Ok(Token::Newline);
            }
            Some(b'#') => {
                self.skip_comment();
                return self.next_token();
            }
            Some(b'|') => {
                self.advance();
                return Ok(if self.peek() == Some(b'|') {
                    self.advance();
                    Token::OrOr
                } else {
                    Token::Pipe
                });
            }
            Some(b'&') => {
                if self.peek2() == Some(b'>') {
                    return self.lex_redir(None);
                }
                self.advance();
                return Ok(if self.peek() == Some(b'&') {
                    self.advance();
                    Token::AndAnd
                } else {
                    Token::Ampersand
                });
            }
            Some(b';') => {
                self.advance();
                return Ok(if self.peek() == Some(b';') {
                    self.advance();
                    Token::SemiSemi
                } else {
                    Token::Semi
                });
            }
            Some(b'(') => {
                self.advance();
                return Ok(Token::LParen);
            }
            Some(b')') => {
                self.advance();
                return Ok(Token::RParen);
            }
            Some(b'{') => {
                self.advance();
                return Ok(Token::LBrace);
            }
            Some(b'}') => {
                self.advance();
                return Ok(Token::RBrace);
            }
            Some(b'!') => {
                self.advance();
                return Ok(Token::Bang);
            }
            Some(b'<' | b'>') => return self.lex_redir(None),
            // [ and ] are standalone word tokens (used by the [ / test builtin).
            Some(b'[') => {
                self.advance();
                return Ok(Token::Word("[".into()));
            }
            Some(b']') => {
                self.advance();
                return Ok(Token::Word("]".into()));
            }
            _ => {}
        }

        let mut buf = String::new();
        self.lex_word_into(&mut buf)?;

        if !buf.is_empty()
            && buf.chars().all(|c| c.is_ascii_digit())
            && matches!(self.peek(), Some(b'<' | b'>'))
        {
            let fd: u32 = buf.parse().unwrap_or(u32::MAX);
            return self.lex_redir(Some(fd));
        }

        Ok(keyword_or_word(buf))
    }
}

fn keyword_or_word(buf: String) -> Token {
    match buf.as_str() {
        "if" => Token::If,
        "then" => Token::Then,
        "else" => Token::Else,
        "elif" => Token::Elif,
        "fi" => Token::Fi,
        "for" => Token::For,
        "in" => Token::In,
        "do" => Token::Do,
        "done" => Token::Done,
        "while" => Token::While,
        "until" => Token::Until,
        "case" => Token::Case,
        "esac" => Token::Esac,
        "function" => Token::Function,
        _ => Token::Word(buf),
    }
}
