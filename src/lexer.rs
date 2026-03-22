use std::fmt;

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
    InOut,
    OutFd,
    BothOut,
    BothAppend,
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Token::Word(w) => write!(f, "Word({w:?})"),
            Token::Pipe => f.write_str("|"),
            Token::OrOr => f.write_str("||"),
            Token::Ampersand => f.write_str("&"),
            Token::AndAnd => f.write_str("&&"),
            Token::Semi => f.write_str(";"),
            Token::SemiSemi => f.write_str(";;"),
            Token::LParen => f.write_str("("),
            Token::RParen => f.write_str(")"),
            Token::LBrace => f.write_str("{"),
            Token::RBrace => f.write_str("}"),
            Token::Bang => f.write_str("!"),
            Token::Newline => f.write_str("\\n"),
            Token::Eof => f.write_str("<EOF>"),
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
        write!(f, "lex error at {}:{} — {}", self.line, self.col, self.msg)
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
    pub fn new(src: &'src str) -> Self {
        Self {
            src: src.as_bytes(),
            pos: 0,
            line: 1,
            col: 1,
        }
    }

    /// Current source position — called by the parser before each token advance
    /// to attach accurate span information.
    #[inline]
    pub fn position(&self) -> (usize, usize) {
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
    fn err(&self, msg: &'static str) -> LexError {
        LexError {
            line: self.line,
            col: self.col,
            msg,
        }
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
                Some(b) => buf.push(b as char),
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
                Some(b) => buf.push(b as char),
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
                        buf.push(n as char);
                    }
                }
                Some(b) => buf.push(b as char),
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
                Some(b) => buf.push(b as char),
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
                        buf.push(n as char);
                    }
                }
                Some(b) => buf.push(b as char),
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
        loop {
            match self.peek() {
                None | Some(b'\n') | Some(b' ') | Some(b'\t') => break,
                Some(b'\'') => {
                    self.advance();
                    self.lex_single_quoted(&mut delim)?;
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
                    // EOF before delimiter — return partial body.
                    None => return Ok(Token::HereString(body)),
                    Some(b'\n') => {
                        self.advance();
                        break;
                    }
                    Some(b) => {
                        line.push(b as char);
                        self.advance();
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

        Ok(Token::HereString(body))
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
                    } else {
                        return self.lex_heredoc(false);
                    }
                }
                Some(b'>') => {
                    self.advance();
                    RedirKind::InOut
                }
                _ => RedirKind::In,
            },
            b'&' => {
                if self.peek() == Some(b'>') {
                    self.advance();
                    if self.peek() == Some(b'>') {
                        self.advance();
                        RedirKind::BothAppend
                    } else {
                        RedirKind::BothOut
                    }
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
                    b' ' | b'\t' | b'\r' | b'\n' | b';' | b'|' | b'&' | b'(' | b')' | b'{' | b'}',
                ) => break,
                Some(b'#') => {
                    if buf.is_empty() {
                        break;
                    } else {
                        buf.push('#');
                        self.advance();
                    }
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
                    self.lex_single_quoted(buf)?;
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
                                Some(b'@' | b'*' | b'#' | b'?' | b'-' | b'$' | b'!') => {
                                    buf.push(self.advance().unwrap() as char);
                                }
                                Some(b'0'..=b'9') => {
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
                Some(b) => {
                    buf.push(b as char);
                    self.advance();
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
            Some(b'<') => return self.lex_redir(None),
            Some(b'>') => return self.lex_redir(None),
            // [[ and ]] are emitted as Word tokens so they hit the builtin table.
            Some(b'[') => {
                self.advance();
                return Ok(if self.peek() == Some(b'[') {
                    self.advance();
                    Token::Word("[[".into())
                } else {
                    Token::Word("[".into())
                });
            }
            Some(b']') => {
                self.advance();
                return Ok(if self.peek() == Some(b']') {
                    self.advance();
                    Token::Word("]]".into())
                } else {
                    Token::Word("]".into())
                });
            }
            _ => {}
        }

        let mut buf = String::new();
        self.lex_word_into(&mut buf)?;

        if !buf.is_empty()
            && buf.chars().all(|c| c.is_ascii_digit())
            && matches!(self.peek(), Some(b'<') | Some(b'>'))
        {
            let fd: u32 = buf.parse().unwrap_or(u32::MAX);
            return self.lex_redir(Some(fd));
        }

        Ok(match buf.as_str() {
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
        })
    }
}
