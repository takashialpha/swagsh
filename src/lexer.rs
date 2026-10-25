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
/// Marks a process substitution in a lexed word: followed by `<` or `>`,
/// then the command as a `$(...)`.
pub const PROC_SUB: char = '\x03';
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
    /// `;&`, ending a `case` arm that falls through.
    SemiAmp,
    /// `;;&`, ending a `case` arm after which matching goes on.
    SemiSemiAmp,
    LParen,
    RParen,
    LBrace,
    RBrace,
    Bang,
    // Redirections
    Redir(RedirToken),
    // Here-documents & here-strings
    HereDoc {
        body: String,
        quoted: bool,
    },
    HereString(String),
    /// `(( expression ))`, with the expression's text.
    ArithCmd(String),
    /// `name=(items)` or `name+=(items)`.
    ArrayAssign(Box<ArrayLiteral>),
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

/// A token inside `[[ ... ]]`, where `(`, `)`, `<` and `>` belong to the
/// expression rather than to the shell, and newlines are blanks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CondToken {
    Word(String),
    And,
    Or,
    Not,
    LParen,
    RParen,
    Less,
    Greater,
    /// The closing `]]`.
    End,
}

/// The text of `name=(items)`: `head` is `name=` or `name+=`, and each item
/// is lexed as a word. Boxed in its token, which it would otherwise make
/// twice as large.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArrayLiteral {
    pub head: String,
    pub items: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedirToken {
    pub kind: RedirKind,
    pub fd: Option<i32>,
    /// `{name}>file`: the variable that gets the descriptor's number.
    /// Boxed, as rare: it would otherwise make every token larger.
    pub fd_var: Option<Box<str>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedirKind {
    Out,
    /// `>|`: like `>`, but overrides `set -C`.
    Clobber,
    Append,
    In,
    /// `<>`: open for reading and writing.
    ReadWrite,
    OutFd,
    InFd,
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
            Self::SemiAmp => f.write_str(";&"),
            Self::SemiSemiAmp => f.write_str(";;&"),
            Self::LParen => f.write_str("("),
            Self::RParen => f.write_str(")"),
            Self::LBrace => f.write_str("{"),
            Self::RBrace => f.write_str("}"),
            Self::Bang => f.write_str("!"),
            Self::Newline => f.write_str("\\n"),
            Self::Eof => f.write_str("<EOF>"),
            Self::HereDoc { .. } => f.write_str("here-doc"),
            Self::ArrayAssign(literal) => write!(f, "{}(", literal.head),
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
    /// Set when this error means "ran out of source before a quote or
    /// substitution closed" rather than a genuinely malformed construct;
    /// the REPL uses this to tell "keep reading more lines" (an unclosed
    /// `'`/`"`/`$(`/`${`/`` ` `` at end of input) apart from a hard error.
    pub incomplete: bool,
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
    /// Delimiter and `<<-` flag of each here-doc lexed on the current line,
    /// in source order. POSIX starts their bodies after the next newline
    /// token, so they are read only once the lexer reaches it.
    pending_heredocs: Vec<(String, bool)>,
    /// A newline has been lexed, so `pending_heredocs` read their bodies
    /// before the next token. Deferred to then, rather than done at the
    /// newline, so a here-doc queued in between (from an alias expanded
    /// after the parser looked ahead past the newline) still gets its body.
    bodies_due: bool,
    heredoc_bodies: Vec<String>,
    /// A here-doc body running into the end of input is reported through
    /// [`Lexer::heredoc_ran_out`] instead of with a warning.
    hold_unterminated: bool,
    heredoc_ran_out: bool,
}

impl<'src> Lexer<'src> {
    #[must_use]
    pub const fn new(src: &'src str) -> Self {
        Self {
            src: src.as_bytes(),
            pos: 0,
            line: 1,
            col: 1,
            pending_heredocs: Vec::new(),
            bodies_due: false,
            heredoc_bodies: Vec::new(),
            hold_unterminated: false,
            heredoc_ran_out: false,
        }
    }

    /// Bodies read since the last call, one per [`Token::HereDoc`] emitted
    /// before the newline that ended them, in the same order. Those tokens
    /// carry an empty `body` until the caller fills it in from here.
    pub fn take_heredoc_bodies(&mut self) -> Vec<String> {
        std::mem::take(&mut self.heredoc_bodies)
    }

    /// Whether only blanks and comments are left.
    pub fn is_exhausted(&mut self) -> bool {
        self.read_due_heredocs();
        loop {
            self.skip_whitespace();
            if self.peek() == Some(b'#') {
                self.skip_comment();
            } else {
                return self.peek().is_none();
            }
        }
    }

    /// Takes the here-docs lexed but not yet given a body, for a lexer that
    /// will not reach another newline (an alias's value).
    pub fn take_pending_heredocs(&mut self) -> Vec<(String, bool)> {
        std::mem::take(&mut self.pending_heredocs)
    }

    /// Queues a here-doc lexed elsewhere (in an alias's value) to take its
    /// body from this lexer's input, as the `at`-th of those still pending.
    pub fn insert_pending_heredoc(&mut self, at: usize, delim: String, strip_tabs: bool) {
        let at = at.min(self.pending_heredocs.len());
        self.pending_heredocs.insert(at, (delim, strip_tabs));
    }

    fn read_due_heredocs(&mut self) {
        if std::mem::take(&mut self.bodies_due) {
            self.read_pending_heredocs();
        }
    }

    fn read_pending_heredocs(&mut self) {
        for (delim, strip_tabs) in std::mem::take(&mut self.pending_heredocs) {
            let body = self.read_heredoc_body(&delim, strip_tabs);
            self.heredoc_bodies.push(body);
        }
    }

    /// Byte offset of the next unread character.
    #[inline]
    #[must_use]
    pub const fn offset(&self) -> usize {
        self.pos
    }

    /// Current source position: called by the parser before each token advance
    /// to attach accurate span information.
    #[inline]
    #[must_use]
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
            incomplete: false,
        }
    }

    /// Like `err`, but for the "source ended before this quote or
    /// substitution closed" case; see `LexError::incomplete`.
    #[inline]
    const fn err_incomplete(&self, msg: &'static str) -> LexError {
        LexError {
            line: self.line,
            col: self.col,
            msg,
            incomplete: true,
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
        // SAFETY: `self.src` is from a valid `&str`, so any run of bytes
        // starting with a non-continuation byte and followed only by
        // continuation bytes is a complete, valid UTF-8 scalar; slicing at
        // these positions is safe.
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
                None => return Err(self.err_incomplete("unterminated single-quoted string")),
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
                None => return Err(self.err_incomplete("unterminated double-quoted string")),
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
                // A `$(...)`/`${...}` here can itself contain quotes (e.g.
                // `"result: $(echo "nested")"`), so it has to be lexed with
                // the same nested-quote/paren-depth tracking used at the
                // top level, not treated as literal text: otherwise the
                // inner `"` reads as this string's own closing quote and
                // cuts everything short.
                Some(b'$') => match self.peek() {
                    Some(b'(') => {
                        self.advance();
                        self.lex_cmd_sub(buf)?;
                    }
                    Some(b'{') => {
                        self.advance();
                        self.lex_param_expand(buf)?;
                    }
                    _ => buf.push('$'),
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
                None => return Err(self.err_incomplete("unterminated command substitution")),
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
                None => return Err(self.err_incomplete("unterminated parameter expansion")),
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
                None => return Err(self.err_incomplete("unterminated backtick substitution")),
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
                None
                | Some(b'\n' | b' ' | b'\t' | b';' | b'|' | b'&' | b'<' | b'>' | b'(' | b')') => {
                    break;
                }
                Some(b'\'') => {
                    self.advance();
                    self.lex_single_quoted(&mut delim)?;
                    quoted = true;
                }
                // POSIX: if *any* character of the delimiter word is
                // quoted, the body is not expanded. Single quotes already
                // set this; double quotes and a backslash escape are the
                // other two spellings and were both being missed, so
                // `<<"EOF"` wrongly expanded `$VAR` in the body.
                Some(b'"') => {
                    self.advance();
                    let start = delim.len();
                    self.lex_double_quoted(&mut delim)?;
                    delim.remove(start);
                    if delim.ends_with('"') {
                        delim.pop();
                    }
                    quoted = true;
                }
                Some(b'\\') => {
                    self.advance();
                    match self.advance() {
                        None => break,
                        Some(b) if b < UTF8_CONT_START => delim.push(b as char),
                        Some(_) => self.push_utf8(&mut delim, self.pos - 1),
                    }
                    quoted = true;
                }
                // `b as char` on a lead byte would mangle a non-ASCII
                // delimiter into mojibake (`é` became `Ã©`), which then
                // never matched the properly decoded body line and ran the
                // here-document to end of input.
                Some(b) if b < UTF8_CONT_START => {
                    self.advance();
                    delim.push(b as char);
                }
                Some(_) => {
                    let start = self.pos;
                    self.advance();
                    self.push_utf8(&mut delim, start);
                }
            }
        }
        if delim.is_empty() {
            return Err(self.err("empty here-doc delimiter"));
        }
        self.pending_heredocs.push((delim, strip_tabs));
        Ok(Token::HereDoc {
            body: String::new(),
            quoted,
        })
    }

    /// Makes a here-doc body that runs into the end of input set
    /// [`Lexer::heredoc_ran_out`] rather than warn, for input that may still
    /// grow, as at the interactive prompt.
    pub const fn hold_unterminated_heredocs(&mut self) {
        self.hold_unterminated = true;
    }

    /// A here-doc body ran into the end of input; see
    /// [`Lexer::hold_unterminated_heredocs`].
    #[must_use]
    pub const fn heredoc_ran_out(&self) -> bool {
        self.heredoc_ran_out
    }

    /// Collects body lines until a line equal to `delim` is found.
    fn read_heredoc_body(&mut self, delim: &str, strip_tabs: bool) -> String {
        let mut body = String::new();
        loop {
            let mut line = String::new();
            let mut at_eof = false;
            loop {
                match self.peek() {
                    None => {
                        at_eof = true;
                        break;
                    }
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
            // Input ran out before the delimiter line ever appeared. Every
            // other shell warns and uses the body it managed to collect, so
            // do the same rather than failing the parse outright. The
            // non-empty check matters: a heredoc whose last line carries no
            // trailing newline still has content here and used to be dropped
            // on the floor, while a body that ended cleanly on a newline
            // leaves `check` empty and must not gain a stray blank line.
            if at_eof {
                if !check.is_empty() {
                    body.push_str(check);
                    body.push('\n');
                }
                if self.hold_unterminated {
                    self.heredoc_ran_out = true;
                } else {
                    crate::errfmt::emit(format!(
                        "warning: here-document delimited by end-of-file (wanted `{delim}`)"
                    ));
                }
                break;
            }
            body.push_str(check);
            body.push('\n');
        }
        body
    }

    // ------------------------------------------------------------------
    // Redirection
    // ------------------------------------------------------------------

    /// Callers only ever invoke this with the cursor on `<`, `>`, or an `&`
    /// that is confirmed (via `peek`/`peek2`) to be followed by `>`; the
    /// error path below exists only to keep this total rather than panic if
    /// that invariant is ever violated.
    fn lex_redir(&mut self, fd: Option<i32>) -> Result<Token, LexError> {
        let kind = match self.advance() {
            Some(b'>') => {
                if self.peek() == Some(b'>') {
                    self.advance();
                    RedirKind::Append
                } else if self.peek() == Some(b'&') {
                    self.advance();
                    RedirKind::OutFd
                } else if self.peek() == Some(b'|') {
                    self.advance();
                    RedirKind::Clobber
                } else {
                    RedirKind::Out
                }
            }
            Some(b'<') => match self.peek() {
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
                Some(b'&') => {
                    self.advance();
                    RedirKind::InFd
                }
                Some(b'>') => {
                    self.advance();
                    RedirKind::ReadWrite
                }
                _ => RedirKind::In,
            },
            Some(b'&') if self.peek() == Some(b'>') => {
                self.advance();
                RedirKind::BothOut
            }
            _ => return Err(self.err("internal error: lex_redir called on a non-redirection")),
        };
        Ok(Token::Redir(RedirToken {
            kind,
            fd,
            fd_var: None,
        }))
    }

    // ------------------------------------------------------------------
    // Word accumulation
    // ------------------------------------------------------------------

    /// # Errors
    ///
    /// Returns an error if a quote or substitution inside the word is
    /// malformed or left unclosed.
    pub fn lex_word_into(&mut self, buf: &mut String) -> Result<(), LexError> {
        self.lex_word(buf, false).map(drop)
    }

    /// Lexes a word into `buf`. With `arrays`, a word `name=` or `name+=`
    /// followed by `(` is an array assignment, whose items are returned.
    ///
    /// Inside the subscript of an assignment word, `a[i + 1]=v`, blanks and
    /// operators are part of the word, as in bash.
    #[inline]
    fn lex_word(
        &mut self,
        buf: &mut String,
        arrays: bool,
    ) -> Result<Option<Vec<String>>, LexError> {
        loop {
            let next = self.peek();
            match next {
                Some(b'[') if crate::expand::is_name(buf) && self.subscript_assignment_ahead() => {
                    self.lex_subscript(buf)?;
                }
                Some(b'(') if arrays && is_array_head(buf) => {
                    return self.lex_array_items().map(Some);
                }
                Some(b'(') if is_extglob_open(buf) => self.lex_extglob_group(buf)?,
                Some(b @ (b'<' | b'>')) if buf.is_empty() && self.peek2() == Some(b'(') => {
                    // `<(cmd)` or `>(cmd)`: a process substitution.
                    buf.push(PROC_SUB);
                    buf.push(b as char);
                    self.advance();
                    self.advance();
                    self.lex_cmd_sub(buf)?;
                }
                None
                | Some(
                    b' ' | b'\t' | b'\r' | b'\n' | b';' | b'|' | b'&' | b'(' | b')' | b'<' | b'>',
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
                        Some(b) if b < UTF8_CONT_START => {
                            buf.push('\\');
                            buf.push(b as char);
                        }
                        // A multi-byte character right after the escaping
                        // backslash: `b` is only its *lead* byte, so
                        // casting it straight to `char` (as the ASCII arm
                        // above does) would both corrupt the character
                        // and orphan its continuation byte(s) at the next
                        // `self.pos`, producing invalid UTF-8 that a later
                        // `unsafe` `str::from_utf8_unchecked` (`push_utf8`)
                        // would then read as if it were valid. Route
                        // through `push_utf8` instead, which consumes the
                        // rest of the sequence properly.
                        Some(_) => {
                            buf.push('\\');
                            self.push_utf8(buf, self.pos - 1);
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
                    self.lex_dollar(buf)?;
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
        Ok(None)
    }

    /// Moves back to byte `offset`, at `line` and `col`, to lex what follows
    /// again in another way (the inside of `[[ ... ]]`).
    pub const fn rewind(&mut self, offset: usize, line: usize, col: usize) {
        self.pos = offset;
        self.line = line;
        self.col = col;
    }

    /// The next token inside `[[ ... ]]`.
    ///
    /// # Errors
    ///
    /// Returns an error at the end of input, where the expression is
    /// incomplete, or on a character that cannot start a token.
    pub fn cond_token(&mut self) -> Result<CondToken, LexError> {
        loop {
            match self.peek() {
                Some(b' ' | b'\t' | b'\r' | b'\n') => {
                    self.advance();
                }
                Some(b'\\') if self.peek2() == Some(b'\n') => {
                    self.advance();
                    self.advance();
                }
                // Blanks were just skipped, so a `#` starts a word: a comment.
                Some(b'#') => self.skip_comment(),
                _ => break,
            }
        }
        let after = |n: usize| self.src.get(self.pos + n).copied();
        let ends = |b: Option<u8>| {
            b.is_none_or(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n' | b';' | b'&' | b'|' | b')'))
        };
        let (token, len) = match self.peek() {
            None => return Err(self.err_incomplete("unterminated `[[`")),
            Some(b']') if after(1) == Some(b']') && ends(after(2)) => (CondToken::End, 2),
            Some(b'(') => (CondToken::LParen, 1),
            Some(b')') => (CondToken::RParen, 1),
            Some(b'&') if after(1) == Some(b'&') => (CondToken::And, 2),
            Some(b'|') if after(1) == Some(b'|') => (CondToken::Or, 2),
            Some(b'<') => (CondToken::Less, 1),
            Some(b'>') => (CondToken::Greater, 1),
            Some(b'!') if ends(after(1)) => (CondToken::Not, 1),
            _ => {
                let mut buf = String::new();
                self.lex_word(&mut buf, false)?;
                if buf.is_empty() {
                    return Err(self.err("unexpected character in `[[`"));
                }
                return Ok(CondToken::Word(buf));
            }
        };
        for _ in 0..len {
            self.advance();
        }
        Ok(token)
    }

    /// The right side of `=~` inside `[[ ... ]]`: one word, in which `(`,
    /// `)` and `|` belong to the regular expression.
    ///
    /// # Errors
    ///
    /// Returns an error if a quote or expansion in it is not closed.
    pub fn cond_regex(&mut self) -> Result<String, LexError> {
        while matches!(self.peek(), Some(b' ' | b'\t')) {
            self.advance();
        }
        let mut buf = String::new();
        let mut depth = 0usize;
        while let Some(b) = self.peek() {
            match b {
                b' ' | b'\t' | b'\r' | b'\n' | b')' if depth == 0 => break,
                b'&' if depth == 0 && self.peek2() == Some(b'&') => break,
                b'(' => depth += 1,
                b')' => depth -= 1,
                b'\'' => {
                    self.advance();
                    buf.push(QUOTE_START);
                    self.lex_single_quoted(&mut buf)?;
                    buf.push(QUOTE_END);
                    continue;
                }
                b'"' => {
                    self.advance();
                    self.lex_double_quoted(&mut buf)?;
                    continue;
                }
                b'$' if self.peek2() != Some(b')')
                    && self.peek2().is_some_and(|n| !n.is_ascii_whitespace()) =>
                {
                    self.advance();
                    self.lex_dollar(&mut buf)?;
                    continue;
                }
                b'\\' => {
                    self.advance();
                    buf.push('\\');
                    if let Some(n) = self.peek() {
                        let start = self.pos;
                        self.advance();
                        if n < UTF8_CONT_START {
                            buf.push(n as char);
                        } else {
                            self.push_utf8(&mut buf, start);
                        }
                    }
                    continue;
                }
                _ => {}
            }
            let start = self.pos;
            self.advance();
            if b < UTF8_CONT_START {
                buf.push(b as char);
            } else {
                self.push_utf8(&mut buf, start);
            }
        }
        Ok(buf)
    }

    /// The rest of a `$'...'` string, after its opening quote: decoded, and
    /// marked as quoted like a single-quoted string.
    fn lex_ansi_c_quoted(&mut self, buf: &mut String) -> Result<(), LexError> {
        let start = self.pos;
        loop {
            match self.advance() {
                None => return Err(self.err_incomplete("unterminated $'...' string")),
                Some(b'\\') => {
                    self.advance();
                }
                Some(b'\'') => break,
                Some(_) => {}
            }
        }
        let raw = String::from_utf8_lossy(&self.src[start..self.pos - 1]);
        buf.push(QUOTE_START);
        buf.push_str(&crate::expand::ansi_c_unescape(&raw));
        buf.push(QUOTE_END);
        Ok(())
    }

    /// `;`, `;;`, `;&` or `;;&`, from the first `;`.
    fn lex_semicolon(&mut self) -> Token {
        self.advance();
        let double = self.peek() == Some(b';');
        if double {
            self.advance();
        }
        if self.peek() == Some(b'&') && self.peek2() != Some(b'&') {
            self.advance();
            return if double {
                Token::SemiSemiAmp
            } else {
                Token::SemiAmp
            };
        }
        if double { Token::SemiSemi } else { Token::Semi }
    }

    /// From a `((`: where the `))` that closes it as an arithmetic command
    /// starts. `None` if the parentheses close some other way, as in the
    /// nested subshells `((cd /tmp) && ls)`.
    fn arith_command_end(&self) -> Option<usize> {
        let mut depth = 0usize;
        let mut i = self.pos + 2;
        while let Some(&b) = self.src.get(i) {
            match b {
                b'(' => depth += 1,
                b')' if depth > 0 => depth -= 1,
                b')' => return (self.src.get(i + 1) == Some(&b')')).then_some(i),
                _ => {}
            }
            i += 1;
        }
        None
    }

    /// What follows a `$` in a word: `$'...'`, `$(...)`, `${...}`, or a
    /// parameter name.
    #[inline]
    fn lex_dollar(&mut self, buf: &mut String) -> Result<(), LexError> {
        match self.peek() {
            Some(b'\'') => {
                self.advance();
                self.lex_ansi_c_quoted(buf)?;
            }
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
                    Some(b'@' | b'*' | b'#' | b'?' | b'-' | b'$' | b'!' | b'0'..=b'9') => {
                        if let Some(b) = self.advance() {
                            buf.push(b as char);
                        }
                    }
                    Some(b'a'..=b'z' | b'A'..=b'Z' | b'_') => {
                        while matches!(
                            self.peek(),
                            Some(b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_')
                        ) {
                            if let Some(b) = self.advance() {
                                buf.push(b as char);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    /// The character after a backslash, taken whole into `buf`.
    fn push_escaped(&mut self, buf: &mut String) {
        let Some(b) = self.peek() else {
            return;
        };
        let start = self.pos;
        self.advance();
        if b < UTF8_CONT_START {
            buf.push(b as char);
        } else {
            self.push_utf8(buf, start);
        }
    }

    /// The subscript of an assignment word, `[i + 1]` in `a[i + 1]=v`, from
    /// its `[` to its `]`: blanks and operators in it are part of the word,
    /// as in bash, while quotes and expansions are lexed as anywhere else.
    fn lex_subscript(&mut self, buf: &mut String) -> Result<(), LexError> {
        let mut depth = 0usize;
        while let Some(b) = self.peek() {
            match b {
                b'\n' => break,
                b'[' => depth += 1,
                b']' => depth -= 1,
                b'\'' => {
                    self.advance();
                    buf.push(QUOTE_START);
                    self.lex_single_quoted(buf)?;
                    buf.push(QUOTE_END);
                    continue;
                }
                b'"' => {
                    self.advance();
                    self.lex_double_quoted(buf)?;
                    continue;
                }
                b'$' => {
                    self.advance();
                    self.lex_dollar(buf)?;
                    continue;
                }
                b'\\' => {
                    buf.push('\\');
                    self.advance();
                    self.push_escaped(buf);
                    continue;
                }
                _ => {}
            }
            let start = self.pos;
            self.advance();
            if b < UTF8_CONT_START {
                buf.push(b as char);
            } else {
                self.push_utf8(buf, start);
            }
            if depth == 0 {
                break;
            }
        }
        Ok(())
    }

    /// The `(...)` of an extended pattern such as `@(a|b)`, from its `(`:
    /// `|` and nested groups belong to the word.
    fn lex_extglob_group(&mut self, buf: &mut String) -> Result<(), LexError> {
        let mut depth = 0usize;
        while let Some(b) = self.peek() {
            match b {
                b'\n' => break,
                b'(' => depth += 1,
                b')' => depth -= 1,
                b'\'' => {
                    self.advance();
                    buf.push(QUOTE_START);
                    self.lex_single_quoted(buf)?;
                    buf.push(QUOTE_END);
                    continue;
                }
                b'"' => {
                    self.advance();
                    self.lex_double_quoted(buf)?;
                    continue;
                }
                b'$' => {
                    self.advance();
                    self.lex_dollar(buf)?;
                    continue;
                }
                b'\\' => {
                    buf.push('\\');
                    self.advance();
                    self.push_escaped(buf);
                    continue;
                }
                _ => {}
            }
            let start = self.pos;
            self.advance();
            if b < UTF8_CONT_START {
                buf.push(b as char);
            } else {
                self.push_utf8(buf, start);
            }
            if depth == 0 {
                break;
            }
        }
        Ok(())
    }

    /// From a `[` after a name: whether a matching `]` followed by `=` or
    /// `+=` comes before the end of the line, making the word an
    /// assignment to an array element.
    fn subscript_assignment_ahead(&self) -> bool {
        let mut depth = 0usize;
        let mut i = self.pos;
        while let Some(&b) = self.src.get(i) {
            match b {
                b'\n' => return false,
                b'\\' => i += 1,
                b'\'' | b'"' => {
                    i += 1;
                    while let Some(&c) = self.src.get(i) {
                        if c == b {
                            break;
                        }
                        if c == b'\\' && b == b'"' {
                            i += 1;
                        }
                        i += 1;
                    }
                }
                b'[' => depth += 1,
                b']' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        let rest = &self.src[i + 1..];
                        return rest.starts_with(b"=") || rest.starts_with(b"+=");
                    }
                }
                _ => {}
            }
            i += 1;
        }
        false
    }

    /// The items of an array assignment, from its `(` to its `)`. They may
    /// span lines and hold comments.
    fn lex_array_items(&mut self) -> Result<Vec<String>, LexError> {
        self.advance();
        let mut items = Vec::new();
        loop {
            match self.peek() {
                Some(b' ' | b'\t' | b'\r' | b'\n') => {
                    self.advance();
                    continue;
                }
                Some(b'\\') if self.peek2() == Some(b'\n') => {
                    self.advance();
                    self.advance();
                    continue;
                }
                Some(b'#') => {
                    self.skip_comment();
                    continue;
                }
                Some(b')') => {
                    self.advance();
                    return Ok(items);
                }
                None => return Err(self.err_incomplete("unterminated array assignment")),
                _ => {}
            }
            let mut item = String::new();
            self.lex_word(&mut item, false)?;
            if item.is_empty() {
                return Err(self.err("unexpected character in array assignment"));
            }
            items.push(item);
        }
    }

    // ------------------------------------------------------------------
    // Public interface
    // ------------------------------------------------------------------

    /// True when the brace under the cursor is a complete word by itself,
    /// i.e. what follows it delimits a word rather than continuing one.
    fn brace_is_own_word(&self) -> bool {
        matches!(
            self.peek2(),
            None | Some(
                b' ' | b'\t' | b'\r' | b'\n' | b';' | b'|' | b'&' | b'(' | b')' | b'<' | b'>'
            )
        )
    }

    /// # Errors
    ///
    /// Returns an error if the next token is malformed (e.g. an unclosed
    /// quote, substitution, or here-doc).
    pub fn next_token(&mut self) -> Result<Token, LexError> {
        self.read_due_heredocs();
        self.skip_whitespace();

        match self.peek() {
            None => {
                self.read_pending_heredocs();
                return Ok(Token::Eof);
            }
            Some(b'\n') => {
                self.advance();
                self.bodies_due = true;
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
            Some(b';') => return Ok(self.lex_semicolon()),
            Some(b'(') => {
                if self.peek2() == Some(b'(')
                    && let Some(end) = self.arith_command_end()
                {
                    let text = String::from_utf8_lossy(&self.src[self.pos + 2..end]).into_owned();
                    while self.pos < end + 2 {
                        self.advance();
                    }
                    return Ok(Token::ArithCmd(text));
                }
                self.advance();
                return Ok(Token::LParen);
            }
            Some(b')') => {
                self.advance();
                return Ok(Token::RParen);
            }
            // `{`/`}` are reserved words only where a command can start
            // and only as a word of their own (`{ list; }`). Glued to other
            // text they are ordinary characters, so `echo a{b}c` is a single
            // literal word: emitting `LBrace`/`RBrace` unconditionally split
            // it into three commands and ran `b` and `c`. Falling through
            // hands them to `lex_word_into`, which no longer breaks on them.
            Some(b'{') if self.brace_is_own_word() => {
                self.advance();
                return Ok(Token::LBrace);
            }
            Some(b'}') if self.brace_is_own_word() => {
                self.advance();
                return Ok(Token::RBrace);
            }
            // Reserved only as a word of its own: `!=` and `!x` are words.
            // `!(...)` is an extended pattern.
            Some(b'!') if self.peek2().is_none_or(|b| b != b'(' && ends_word(b)) => {
                self.advance();
                return Ok(Token::Bang);
            }
            Some(b'<' | b'>') if self.peek2() != Some(b'(') => return self.lex_redir(None),
            _ => {}
        }

        let mut buf = String::new();
        if let Some(items) = self.lex_word(&mut buf, true)? {
            return Ok(Token::ArrayAssign(Box::new(ArrayLiteral {
                head: buf,
                items,
            })));
        }

        // A leading digit run only introduces a redirection if it actually
        // fits a file descriptor. `9999999999>file` is not fd 9999999999:
        // bash treats an out-of-range run as an ordinary command word (and
        // then reports "command not found"), which is exactly what falling
        // through to `keyword_or_word` below does. Saturating to `u32::MAX`
        // instead, as this used to, reached the parser as fd -1 once signed,
        // and `OwnedFd::from_raw_fd(-1)` panicked, aborting the shell.
        if !buf.is_empty()
            && buf.chars().all(|c| c.is_ascii_digit())
            && matches!(self.peek(), Some(b'<' | b'>'))
            && let Ok(fd) = buf.parse::<i32>()
        {
            return self.lex_redir(Some(fd));
        }
        // `{name}>file`: the shell picks the descriptor and sets `name`.
        if matches!(self.peek(), Some(b'<' | b'>'))
            && let Some(name) = buf.strip_prefix('{').and_then(|b| b.strip_suffix('}'))
            && crate::expand::is_name(name.split_once('[').map_or(name, |(n, _)| n))
            && (!name.contains('[') || name.ends_with(']'))
        {
            let mut token = self.lex_redir(None)?;
            if let Token::Redir(redir) = &mut token {
                redir.fd_var = Some(name.into());
            }
            return Ok(token);
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

/// `name=` or `name+=`: what may come right before an array assignment's
/// `(`.
fn is_array_head(buf: &str) -> bool {
    buf.strip_suffix('=')
        .map(|n| n.strip_suffix('+').unwrap_or(n))
        .is_some_and(crate::expand::is_name)
}

/// A byte that ends an unquoted word.
const fn ends_word(b: u8) -> bool {
    matches!(
        b,
        b' ' | b'\t' | b'\r' | b'\n' | b';' | b'&' | b'|' | b'(' | b')' | b'<' | b'>'
    )
}

/// A word so far ending in one of `? * + @ !` (not a `$?`, `$*`, ... before
/// it), which a `(` turns into an extended pattern group.
fn is_extglob_open(buf: &str) -> bool {
    let mut rev = buf.chars().rev();
    matches!(rev.next(), Some('?' | '*' | '+' | '@' | '!')) && rev.next() != Some('$')
}
