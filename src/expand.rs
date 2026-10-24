use crate::env::Env;

// ---------------------------------------------------------------------------
// Shell-quoting (for builtins that print back values a user might re-source,
// e.g. `export`/`alias`/`set` with no arguments)
// ---------------------------------------------------------------------------

/// Quotes `s` so it round-trips as a single shell word if pasted back in.
///
/// Unquoted when every byte is already safe bare, single-quoted (with
/// embedded `'` escaped via the standard `'\''` close-escape-reopen
/// sequence) otherwise. Always single-quoting unconditionally would also be
/// correct but produces noisy output for the common case of a plain value.
#[must_use]
pub fn shell_quote(s: &str) -> String {
    let is_bare_safe = |b: u8| {
        b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'/' | b':' | b'=' | b'@')
    };
    if !s.is_empty() && s.bytes().all(is_bare_safe) {
        return s.to_owned();
    }
    shell_quote_always(s)
}

/// Like [`shell_quote`], but always wraps in single quotes even when `s` is
/// bare-safe, matching `alias`'s own always-quoted display convention.
#[must_use]
pub fn shell_quote_always(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

// ---------------------------------------------------------------------------
// Tilde expansion
// ---------------------------------------------------------------------------

/// Expands a leading `~` (`$HOME`) or `~user` (that user's home directory)
/// up to the first `/`. An unknown user leaves `s` unchanged.
/// Borrows `s` when there is nothing to expand.
#[must_use]
pub fn expand_tilde<'a>(s: &'a str, env: &Env) -> std::borrow::Cow<'a, str> {
    let Some(rest) = s.strip_prefix('~') else {
        return s.into();
    };
    let (user, tail) = rest.find('/').map_or((rest, ""), |i| rest.split_at(i));
    let home = if user.is_empty() {
        env.get("HOME").unwrap_or_else(|| "/".into())
    } else {
        match crate::sys::home_dir_of(user) {
            Some(home) => home,
            None => return s.into(),
        }
    };
    format!("{home}{tail}").into()
}

// ---------------------------------------------------------------------------
// Glob
// ---------------------------------------------------------------------------

/// Expands a pathname pattern against the filesystem, sorted.
///
/// Works one `/`-separated component at a time, so `a/*/b` and `/*/bin`
/// work, and returns nothing if there are no matches. A leading `.` in a
/// name is only matched by a component that itself starts with `.`.
#[must_use]
pub fn glob_expand(pattern: &str) -> Vec<String> {
    let components: Vec<&str> = pattern.split('/').filter(|c| !c.is_empty()).collect();
    let dirs_only = pattern.ends_with('/');
    let mut paths = vec![if pattern.starts_with('/') {
        "/".to_owned()
    } else {
        String::new()
    }];
    for (i, component) in components.iter().enumerate() {
        let need_dir = dirs_only || i + 1 < components.len();
        let mut next = Vec::new();
        for base in &paths {
            if !has_glob_syntax(component) {
                let path = join_path(base, &unescape_glob(component));
                if path_exists(&path, need_dir) {
                    next.push(path);
                }
                continue;
            }
            let dir = if base.is_empty() { "." } else { base.as_str() };
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            let show_hidden = component.starts_with('.') || component.starts_with("\\.");
            for entry in entries.filter_map(Result::ok) {
                let name = entry.file_name().to_string_lossy().into_owned();
                if (show_hidden || !name.starts_with('.')) && glob_match(component, &name) {
                    let path = join_path(base, &name);
                    if !need_dir || path_exists(&path, true) {
                        next.push(path);
                    }
                }
            }
        }
        paths = next;
        if paths.is_empty() {
            break;
        }
    }
    if dirs_only {
        for path in &mut paths {
            path.push('/');
        }
    }
    paths.sort();
    paths
}

fn join_path(base: &str, name: &str) -> String {
    if base.is_empty() {
        name.to_owned()
    } else if base.ends_with('/') {
        format!("{base}{name}")
    } else {
        format!("{base}/{name}")
    }
}

fn path_exists(path: &str, as_dir: bool) -> bool {
    let path = std::path::Path::new(path);
    if as_dir {
        path.is_dir()
    } else {
        path.symlink_metadata().is_ok()
    }
}

/// Whether `pattern` could match anything other than itself.
///
/// True for an unescaped `*` or `?`, or an unescaped `[` closed by a later
/// `]`. A lone `[` (the test command) is not a pattern, so it never costs a
/// directory scan.
#[must_use]
pub fn has_glob_syntax(pattern: &str) -> bool {
    let mut chars = pattern.chars();
    let mut open_bracket = false;
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                chars.next();
            }
            '*' | '?' => return true,
            '[' => open_bracket = true,
            ']' if open_bracket => return true,
            _ => {}
        }
    }
    false
}

/// Removes the backslash escapes [`escape_glob`] adds.
fn unescape_glob(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            out.extend(chars.next());
        } else {
            out.push(c);
        }
    }
    out
}

/// Iterative, not the textbook recursive-backtracking `*`/`?` matcher.
///
/// The recursive version (`glob_inner(&p[1..], t) || glob_inner(p, &t[1..])`
/// on a `*`) is exponential on adversarial input, e.g. pattern `*a*a*a*...`
/// against a text that almost-but-doesn't match re-explores the same
/// suffix under every placement of every `*` (found by fuzzing
/// `param_expand`: a 25-star pattern against a 30-character text took ~6
/// CPU-seconds). This shape of pattern is entirely ordinary `case`/
/// `${var#pattern}` input, not something anyone has to construct
/// adversarially, so it's a real hang, not just a theoretical one.
///
/// This is the standard single-pass wildcard-matching algorithm instead:
/// walk `text` once, and on a mismatch, backtrack only to the *last* `*`
/// (advancing how much of `text` it's asked to consume by one) rather than
/// re-deriving the whole match from scratch. Bounded by `pattern.len() *
/// text.len()` work in the worst case: polynomial, not exponential.
///
/// Besides `*` and `?`, a bracket expression (`[abc]`, `[a-z]`, `[!x]`,
/// `[[:alpha:]]`) matches one character from a set; a `[` with no closing
/// `]` matches itself. A backslash makes the character after it match only
/// itself; see [`escape_glob`].
#[must_use]
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();

    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut star_match = 0usize;

    while ti < t.len() {
        if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            star_match = ti;
            pi += 1;
            continue;
        }
        if pi + 1 < p.len() && p[pi] == '\\' {
            if p[pi + 1] == t[ti] {
                pi += 2;
                ti += 1;
                continue;
            }
        } else if let Some((matched, next)) = (p.get(pi) == Some(&'['))
            .then(|| match_bracket(&p, pi, t[ti]))
            .flatten()
        {
            if matched {
                pi = next;
                ti += 1;
                continue;
            }
        } else if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
            continue;
        }
        if let Some(si) = star {
            pi = si + 1;
            star_match += 1;
            ti = star_match;
        } else {
            return false;
        }
    }

    while p.get(pi) == Some(&'*') {
        pi += 1;
    }
    pi == p.len()
}

/// Matches `c` against the bracket expression opening at `p[start]`,
/// returning whether it matched and the index just past the closing `]`, or
/// `None` if the expression is never closed.
fn match_bracket(p: &[char], start: usize, c: char) -> Option<(bool, usize)> {
    let mut i = start + 1;
    let negate = matches!(p.get(i), Some('!' | '^'));
    if negate {
        i += 1;
    }
    let mut matched = false;
    let mut first = true;
    loop {
        let ch = *p.get(i)?;
        if ch == ']' && !first {
            return Some((matched != negate, i + 1));
        }
        first = false;
        if ch == '['
            && p.get(i + 1) == Some(&':')
            && let Some(len) = p[i + 2..].windows(2).position(|w| w == [':', ']'])
        {
            let class: String = p[i + 2..i + 2 + len].iter().collect();
            matched |= char_in_class(&class, c);
            i += len + 4;
            continue;
        }
        let (lo, after_lo) = bracket_char(p, i)?;
        if p.get(after_lo) == Some(&'-') && p.get(after_lo + 1).is_some_and(|&x| x != ']') {
            let (hi, after_hi) = bracket_char(p, after_lo + 1)?;
            matched |= (lo..=hi).contains(&c);
            i = after_hi;
        } else {
            matched |= c == lo;
            i = after_lo;
        }
    }
}

/// The (possibly backslash-escaped) character at `p[i]` and the index after it.
fn bracket_char(p: &[char], i: usize) -> Option<(char, usize)> {
    match p.get(i)? {
        '\\' => Some((*p.get(i + 1)?, i + 2)),
        &c => Some((c, i + 1)),
    }
}

fn char_in_class(class: &str, c: char) -> bool {
    match class {
        "alpha" => c.is_alphabetic(),
        "digit" => c.is_ascii_digit(),
        "alnum" => c.is_alphanumeric(),
        "upper" => c.is_uppercase(),
        "lower" => c.is_lowercase(),
        "space" => c.is_whitespace(),
        "blank" => c == ' ' || c == '\t',
        "punct" => c.is_ascii_punctuation(),
        "print" => !c.is_control(),
        "graph" => !c.is_control() && !c.is_whitespace(),
        "cntrl" => c.is_control(),
        "xdigit" => c.is_ascii_hexdigit(),
        _ => false,
    }
}

/// Backslash-escapes the characters [`glob_match`] treats specially, so
/// that `s` matches only itself.
#[must_use]
pub fn escape_glob(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '*' | '?' | '[' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

// ---------------------------------------------------------------------------
// Escape sequences (for echo -e and printf)
// ---------------------------------------------------------------------------

/// Consumes up to `max` leading ASCII hex digits from `chars`, stopping
/// early at the first non-hex character (`\x`/`\u`/`\U` are all non-greedy
/// this way, e.g. `\u41 ` is just `A` followed by a space).
fn take_hex(chars: &mut std::iter::Peekable<std::str::Chars<'_>>, max: usize) -> String {
    let mut hex = String::new();
    while hex.len() < max {
        match chars.next_if(char::is_ascii_hexdigit) {
            Some(c) => hex.push(c),
            None => break,
        }
    }
    hex
}

/// Expands `echo -e`/`printf`'s backslash escapes.
///
/// Returns the expanded text and whether a `\c` was seen: that escape means
/// "stop all further output here", including any trailing newline `echo`
/// would otherwise add, so callers can't just treat it as a character to
/// insert.
#[must_use]
pub fn unescape(s: &str) -> (String, bool) {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.peek().copied() {
            Some('n') => {
                chars.next();
                out.push('\n');
            }
            Some('t') => {
                chars.next();
                out.push('\t');
            }
            Some('r') => {
                chars.next();
                out.push('\r');
            }
            Some('a') => {
                chars.next();
                out.push('\x07');
            }
            Some('b') => {
                chars.next();
                out.push('\x08');
            }
            Some('e') => {
                chars.next();
                out.push('\x1b');
            }
            Some('f') => {
                chars.next();
                out.push('\x0c');
            }
            Some('v') => {
                chars.next();
                out.push('\x0b');
            }
            Some('\\') => {
                chars.next();
                out.push('\\');
            }
            Some('c') => {
                chars.next();
                return (out, true);
            }
            Some('0') => {
                chars.next();
                let mut value: u32 = 0;
                let mut digits = 0;
                while digits < 3
                    && let Some(d) = chars.peek().and_then(|c| c.to_digit(8))
                {
                    value = value * 8 + d;
                    chars.next();
                    digits += 1;
                }
                out.push(char::from_u32(value).unwrap_or('\0'));
            }
            Some(marker @ ('x' | 'u' | 'U')) => {
                chars.next();
                let max_digits = if marker == 'x' {
                    2
                } else if marker == 'u' {
                    4
                } else {
                    8
                };
                let hex = take_hex(&mut chars, max_digits);
                if hex.is_empty() {
                    out.push('\\');
                    out.push(marker);
                } else {
                    let value = u32::from_str_radix(&hex, 16).unwrap_or(0);
                    out.push(char::from_u32(value).unwrap_or('\u{FFFD}'));
                }
            }
            Some(other) => {
                chars.next();
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    (out, false)
}

// ---------------------------------------------------------------------------
// Parameter expansion operators: ${#var}, ${var#pat}, ${var##pat},
// ${var%pat}, ${var%%pat}, ${var:-word}, ${var:=word}, ${var:?msg}, ${var:+word},
// and the colon-less forms of the last four
// ---------------------------------------------------------------------------

pub enum ParamOp<'a> {
    Length(&'a str),
    PrefixStrip {
        var: &'a str,
        pat: &'a str,
        greedy: bool,
    },
    SuffixStrip {
        var: &'a str,
        pat: &'a str,
        greedy: bool,
    },
    Conditional {
        var: &'a str,
        op: &'a str,
        word: &'a str,
    },
}

/// A variable name, a positional parameter number, or one special parameter.
fn is_param_name(s: &str) -> bool {
    matches!(s, "@" | "*" | "#" | "?" | "-" | "$" | "!")
        || s.chars().all(|c| c.is_ascii_digit()) && !s.is_empty()
        || s.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[must_use]
pub fn parse_param_op(s: &str) -> Option<ParamOp<'_>> {
    // ${#var}: length, starts with '#' followed by a valid name or special param
    if let Some(var) = s.strip_prefix('#')
        && is_param_name(var)
    {
        return Some(ParamOp::Length(var));
    }

    // Find end of the variable name portion: a name, a positional number,
    // or one special parameter character.
    let name_end = if s.starts_with(['@', '*', '#', '?', '-', '$', '!']) {
        1
    } else {
        s.char_indices()
            .take_while(|(_, c)| c.is_ascii_alphanumeric() || *c == '_')
            .last()
            .map_or(0, |(i, c)| i + c.len_utf8())
    };

    // Special single-char parameters have no operators after them in this path
    if name_end == 0 || name_end == s.len() {
        return None;
    }

    let var = &s[..name_end];
    let rest = &s[name_end..];

    if let Some(pat) = rest.strip_prefix("##") {
        return Some(ParamOp::PrefixStrip {
            var,
            pat,
            greedy: true,
        });
    }
    if let Some(pat) = rest.strip_prefix('#') {
        return Some(ParamOp::PrefixStrip {
            var,
            pat,
            greedy: false,
        });
    }
    if let Some(pat) = rest.strip_prefix("%%") {
        return Some(ParamOp::SuffixStrip {
            var,
            pat,
            greedy: true,
        });
    }
    if let Some(pat) = rest.strip_prefix('%') {
        return Some(ParamOp::SuffixStrip {
            var,
            pat,
            greedy: false,
        });
    }
    for op in [":-", ":+", ":?", ":=", "-", "+", "?", "="] {
        if let Some(word) = rest.strip_prefix(op) {
            return Some(ParamOp::Conditional { var, op, word });
        }
    }

    None
}

/// `${var#pat}`/`${var%pat}`-family patterns are ordinary text far more
/// often than they're an actual glob (`${path%.txt}`, `${url#http://}`,
/// ...): without any of `glob_match`'s special characters, `pat` can only
/// ever match at one specific length, greedy or not, and
/// `strip_prefix`/`strip_suffix` below use this to skip the candidate
/// search entirely in that case.
fn has_glob_meta(pat: &str) -> bool {
    pat.contains(['*', '?', '[', '\\'])
}

/// Byte offsets of every char boundary in `s`, including its end: the
/// candidate split points `strip_prefix`/`strip_suffix`'s glob-pattern
/// path below tries, one per loop iteration.
fn char_boundaries(s: &str) -> impl DoubleEndedIterator<Item = usize> {
    s.char_indices().map(|(i, _)| i).chain([s.len()])
}

#[must_use]
pub fn strip_prefix(val: &str, pat: &str, greedy: bool) -> String {
    if !has_glob_meta(pat) {
        return val.strip_prefix(pat).unwrap_or(val).to_owned();
    }
    // Slices of the original `val`, not a `Vec<char>` collected fresh
    // (and re-collected into a new `String` per candidate): the length
    // being tried changes every iteration, but the text underneath it
    // never does, so there's nothing to copy until an actual match is
    // found.
    let boundaries: Vec<usize> = char_boundaries(val).collect();
    let candidates: Box<dyn Iterator<Item = &usize>> = if greedy {
        Box::new(boundaries.iter().rev())
    } else {
        Box::new(boundaries.iter())
    };
    for &len in candidates {
        if glob_match(pat, &val[..len]) {
            return val[len..].to_owned();
        }
    }
    val.to_owned()
}

#[must_use]
pub fn strip_suffix(val: &str, pat: &str, greedy: bool) -> String {
    if !has_glob_meta(pat) {
        return val.strip_suffix(pat).unwrap_or(val).to_owned();
    }
    let boundaries: Vec<usize> = char_boundaries(val).collect();
    let candidates: Box<dyn Iterator<Item = &usize>> = if greedy {
        Box::new(boundaries.iter())
    } else {
        Box::new(boundaries.iter().rev())
    };
    for &start in candidates {
        if glob_match(pat, &val[start..]) {
            return val[..start].to_owned();
        }
    }
    val.to_owned()
}

// ---------------------------------------------------------------------------
// Arithmetic expansion: $(( expr ))
// POSIX (C-style) integer arithmetic on `i64`: decimal, octal (`010`) and hex
// (`0x10`) constants, variables by name, and every C operator except `++`,
// `--`, `,`, and `sizeof`. Overflow wraps. `$` references have already been
// expanded by the caller.
// ---------------------------------------------------------------------------

/// Why an arithmetic expression could not be evaluated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArithError(pub String);

impl std::fmt::Display for ArithError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "arithmetic: {}", self.0)
    }
}

impl std::error::Error for ArithError {}

/// Where an arithmetic expression reads and assigns its variables.
pub trait ArithVars {
    fn get_var(&self, name: &str) -> Option<&str>;
    /// # Errors
    ///
    /// Returns an error if `name` cannot be assigned (it is readonly).
    fn set_var(&mut self, name: &str, value: i64) -> Result<(), ArithError>;
}

impl ArithVars for Env {
    fn get_var(&self, name: &str) -> Option<&str> {
        self.get_ref(name)
    }
    fn set_var(&mut self, name: &str, value: i64) -> Result<(), ArithError> {
        self.assign(name, value.to_string())
            .map_err(|e| ArithError(e.to_string()))
    }
}

/// Evaluates `expr` with every variable unset.
///
/// # Errors
///
/// Returns an error if `expr` is malformed or divides by zero.
pub fn eval_arith(expr: &str) -> Result<i64, ArithError> {
    struct NoVars;
    impl ArithVars for NoVars {
        fn get_var(&self, _: &str) -> Option<&str> {
            None
        }
        fn set_var(&mut self, _: &str, _: i64) -> Result<(), ArithError> {
            Ok(())
        }
    }
    eval_arith_with(expr, &mut NoVars)
}

/// Evaluates `expr`, reading and assigning variables through `vars`.
///
/// # Errors
///
/// Returns an error if `expr` is malformed, divides by zero, or reads a
/// variable whose value is not an integer constant.
pub fn eval_arith_with(expr: &str, vars: &mut dyn ArithVars) -> Result<i64, ArithError> {
    let tokens = arith_tokenize(expr)?;
    if tokens.is_empty() {
        return Ok(0);
    }
    let mut parser = ArithParser {
        tokens: &tokens,
        pos: 0,
        depth: 0,
        vars,
    };
    let value = parser.assign(true)?;
    if parser.pos != tokens.len() {
        return Err(ArithError(format!("unexpected token in `{}`", expr.trim())));
    }
    Ok(value)
}

/// Parses an integer constant: decimal, octal with a leading `0`, or hex with
/// `0x`/`0X`, optionally signed and surrounded by blanks (a variable's value
/// is read this way).
fn parse_arith_number(s: &str) -> Option<i64> {
    let s = s.trim_matches([' ', '\t', '\n']);
    let negative = s.starts_with('-');
    let digits = s.strip_prefix(['-', '+']).unwrap_or(s);
    let (radix, digits) = match digits.as_bytes() {
        [b'0', b'x' | b'X', ..] => (16, &digits[2..]),
        [b'0', _, ..] => (8, &digits[1..]),
        _ => (10, digits),
    };
    if digits.is_empty() {
        return None;
    }
    let mut n: i64 = 0;
    for c in digits.chars() {
        let d = c.to_digit(radix)?;
        n = n.wrapping_mul(i64::from(radix)).wrapping_add(i64::from(d));
    }
    Some(if negative { n.wrapping_neg() } else { n })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BinOp {
    Mul,
    Div,
    Rem,
    Add,
    Sub,
    Shl,
    Shr,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
    BitAnd,
    BitXor,
    BitOr,
    And,
    Or,
}

impl BinOp {
    /// Binding strength; higher binds tighter.
    const fn precedence(self) -> u8 {
        match self {
            Self::Mul | Self::Div | Self::Rem => 10,
            Self::Add | Self::Sub => 9,
            Self::Shl | Self::Shr => 8,
            Self::Lt | Self::Le | Self::Gt | Self::Ge => 7,
            Self::Eq | Self::Ne => 6,
            Self::BitAnd => 5,
            Self::BitXor => 4,
            Self::BitOr => 3,
            Self::And => 2,
            Self::Or => 1,
        }
    }

    fn apply(self, a: i64, b: i64) -> Result<i64, ArithError> {
        let shift = || u32::try_from(b & 63).unwrap_or(0);
        Ok(match self {
            Self::Mul => a.wrapping_mul(b),
            Self::Div | Self::Rem if b == 0 => {
                return Err(ArithError("division by zero".to_owned()));
            }
            Self::Div => a.wrapping_div(b),
            Self::Rem => a.wrapping_rem(b),
            Self::Add => a.wrapping_add(b),
            Self::Sub => a.wrapping_sub(b),
            Self::Shl => a.wrapping_shl(shift()),
            Self::Shr => a.wrapping_shr(shift()),
            Self::Lt => i64::from(a < b),
            Self::Le => i64::from(a <= b),
            Self::Gt => i64::from(a > b),
            Self::Ge => i64::from(a >= b),
            Self::Eq => i64::from(a == b),
            Self::Ne => i64::from(a != b),
            Self::BitAnd => a & b,
            Self::BitXor => a ^ b,
            Self::BitOr => a | b,
            Self::And => i64::from(a != 0 && b != 0),
            Self::Or => i64::from(a != 0 || b != 0),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tok<'a> {
    Num(i64),
    Ident(&'a str),
    Bin(BinOp),
    /// `=` or a compound assignment such as `+=`.
    Assign(Option<BinOp>),
    Bang,
    Tilde,
    Question,
    Colon,
    LParen,
    RParen,
}

fn arith_tokenize(s: &str) -> Result<Vec<Tok<'_>>, ArithError> {
    let mut toks = Vec::new();
    let mut chars = s.char_indices().peekable();
    while let Some((start, c)) = chars.next() {
        let op = match c {
            ' ' | '\t' | '\n' => continue,
            '0'..='9' => {
                let end = take_while_end(&mut chars, s.len(), |c| c.is_ascii_alphanumeric());
                let text = &s[start..end];
                let n = parse_arith_number(text)
                    .ok_or_else(|| ArithError(format!("invalid number `{text}`")))?;
                toks.push(Tok::Num(n));
                continue;
            }
            'a'..='z' | 'A'..='Z' | '_' => {
                let end = take_while_end(&mut chars, s.len(), |c| {
                    c.is_ascii_alphanumeric() || c == '_'
                });
                toks.push(Tok::Ident(&s[start..end]));
                continue;
            }
            '(' => Tok::LParen,
            ')' => Tok::RParen,
            '?' => Tok::Question,
            ':' => Tok::Colon,
            '~' => Tok::Tilde,
            '!' if chars.next_if(|&(_, c)| c == '=').is_some() => Tok::Bin(BinOp::Ne),
            '!' => Tok::Bang,
            '=' if chars.next_if(|&(_, c)| c == '=').is_some() => Tok::Bin(BinOp::Eq),
            '=' => Tok::Assign(None),
            '&' if chars.next_if(|&(_, c)| c == '&').is_some() => Tok::Bin(BinOp::And),
            '|' if chars.next_if(|&(_, c)| c == '|').is_some() => Tok::Bin(BinOp::Or),
            '<' if chars.next_if(|&(_, c)| c == '<').is_some() => Tok::Bin(BinOp::Shl),
            '>' if chars.next_if(|&(_, c)| c == '>').is_some() => Tok::Bin(BinOp::Shr),
            '<' if chars.next_if(|&(_, c)| c == '=').is_some() => Tok::Bin(BinOp::Le),
            '>' if chars.next_if(|&(_, c)| c == '=').is_some() => Tok::Bin(BinOp::Ge),
            '<' => Tok::Bin(BinOp::Lt),
            '>' => Tok::Bin(BinOp::Gt),
            '*' => Tok::Bin(BinOp::Mul),
            '/' => Tok::Bin(BinOp::Div),
            '%' => Tok::Bin(BinOp::Rem),
            '+' => Tok::Bin(BinOp::Add),
            '-' => Tok::Bin(BinOp::Sub),
            '&' => Tok::Bin(BinOp::BitAnd),
            '^' => Tok::Bin(BinOp::BitXor),
            '|' => Tok::Bin(BinOp::BitOr),
            other => return Err(ArithError(format!("unexpected character `{other}`"))),
        };
        // `op=` is a compound assignment for every binary operator that has
        // one; `==`, `<=`, `>=` and `!=` were already consumed above.
        let op = match op {
            Tok::Bin(bin)
                if !matches!(
                    bin,
                    BinOp::And
                        | BinOp::Or
                        | BinOp::Eq
                        | BinOp::Ne
                        | BinOp::Lt
                        | BinOp::Gt
                        | BinOp::Le
                        | BinOp::Ge
                ) && chars.next_if(|&(_, c)| c == '=').is_some() =>
            {
                Tok::Assign(Some(bin))
            }
            op => op,
        };
        toks.push(op);
    }
    Ok(toks)
}

/// Nesting beyond this is rejected rather than risking the stack.
const MAX_ARITH_DEPTH: usize = 256;

/// Consumes characters while `keep` holds, returning the byte offset just
/// past the last one taken.
fn take_while_end(
    chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>,
    len: usize,
    keep: impl Fn(char) -> bool,
) -> usize {
    while chars.next_if(|&(_, c)| keep(c)).is_some() {}
    chars.peek().map_or(len, |&(i, _)| i)
}

/// An unset or empty variable counts as `0`; anything else must be an
/// integer constant.
fn read_arith_var(vars: &dyn ArithVars, name: &str) -> Result<i64, ArithError> {
    match vars.get_var(name) {
        None => Ok(0),
        Some(v) if v.trim_matches([' ', '\t', '\n']).is_empty() => Ok(0),
        Some(v) => {
            parse_arith_number(v).ok_or_else(|| ArithError(format!("{name}: invalid number `{v}`")))
        }
    }
}

/// Evaluates as it parses. Every method takes `eval`: false on the side of
/// `&&`, `||` or `?:` that is not taken, which is still parsed but neither
/// reads nor assigns variables, nor fails on division by zero.
struct ArithParser<'t, 'v> {
    tokens: &'t [Tok<'t>],
    pos: usize,
    depth: usize,
    vars: &'v mut dyn ArithVars,
}

impl<'t> ArithParser<'t, '_> {
    fn peek(&self) -> Option<Tok<'t>> {
        self.tokens.get(self.pos).copied()
    }

    fn eat(&mut self, tok: Tok<'_>) -> bool {
        let found = self.peek() == Some(tok);
        if found {
            self.pos += 1;
        }
        found
    }

    fn nested<T>(
        &mut self,
        f: impl FnOnce(&mut Self) -> Result<T, ArithError>,
    ) -> Result<T, ArithError> {
        self.depth += 1;
        if self.depth > MAX_ARITH_DEPTH {
            return Err(ArithError("expression too deeply nested".to_owned()));
        }
        let result = f(self);
        self.depth -= 1;
        result
    }

    /// `assignment := IDENT ('=' | 'op=') assignment | conditional`
    fn assign(&mut self, eval: bool) -> Result<i64, ArithError> {
        self.nested(|p| {
            let (Some(Tok::Ident(name)), Some(Tok::Assign(op))) =
                (p.peek(), p.tokens.get(p.pos + 1).copied())
            else {
                return p.conditional(eval);
            };
            p.pos += 2;
            let rhs = p.assign(eval)?;
            if !eval {
                return Ok(0);
            }
            let value = match op {
                Some(op) => op.apply(read_arith_var(p.vars, name)?, rhs)?,
                None => rhs,
            };
            p.vars.set_var(name, value)?;
            Ok(value)
        })
    }

    /// `conditional := binary ('?' assignment ':' conditional)?`
    fn conditional(&mut self, eval: bool) -> Result<i64, ArithError> {
        let cond = self.binary(1, eval)?;
        if !self.eat(Tok::Question) {
            return Ok(cond);
        }
        let then = self.assign(eval && cond != 0)?;
        if !self.eat(Tok::Colon) {
            return Err(ArithError("expected `:` in conditional".to_owned()));
        }
        let otherwise = self.nested(|p| p.conditional(eval && cond == 0))?;
        Ok(if cond != 0 { then } else { otherwise })
    }

    /// Precedence climbing over the left-associative binary operators.
    fn binary(&mut self, min_prec: u8, eval: bool) -> Result<i64, ArithError> {
        let mut lhs = self.unary(eval)?;
        while let Some(Tok::Bin(op)) = self.peek() {
            if op.precedence() < min_prec {
                break;
            }
            self.pos += 1;
            let rhs_eval = match op {
                BinOp::And => eval && lhs != 0,
                BinOp::Or => eval && lhs == 0,
                _ => eval,
            };
            let rhs = self.binary(op.precedence() + 1, rhs_eval)?;
            lhs = if eval { op.apply(lhs, rhs)? } else { 0 };
        }
        Ok(lhs)
    }

    fn unary(&mut self, eval: bool) -> Result<i64, ArithError> {
        let Some(op @ (Tok::Bin(BinOp::Sub | BinOp::Add) | Tok::Bang | Tok::Tilde)) = self.peek()
        else {
            return self.primary(eval);
        };
        self.pos += 1;
        let value = self.nested(|p| p.unary(eval))?;
        Ok(match op {
            Tok::Bin(BinOp::Sub) => value.wrapping_neg(),
            Tok::Bang => i64::from(value == 0),
            Tok::Tilde => !value,
            _ => value,
        })
    }

    fn primary(&mut self, eval: bool) -> Result<i64, ArithError> {
        let value = match self.peek() {
            Some(Tok::Num(n)) => n,
            Some(Tok::Ident(name)) if eval => read_arith_var(self.vars, name)?,
            Some(Tok::Ident(_)) => 0,
            Some(Tok::LParen) => {
                self.pos += 1;
                let inner = self.assign(eval)?;
                if !self.eat(Tok::RParen) {
                    return Err(ArithError("expected `)`".to_owned()));
                }
                return Ok(inner);
            }
            _ => return Err(ArithError("expected a number or variable".to_owned())),
        };
        self.pos += 1;
        Ok(value)
    }
}
