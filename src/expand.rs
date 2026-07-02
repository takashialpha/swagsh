use crate::env::Env;

// ---------------------------------------------------------------------------
// Shell-quoting (for builtins that print back values a user might re-source,
// e.g. `export`/`alias`/`set` with no arguments)
// ---------------------------------------------------------------------------

/// Quotes `s` so it round-trips as a single shell word if pasted back in:
/// unquoted when every byte is already safe bare, single-quoted (with
/// embedded `'` escaped via the standard `'\''` close-escape-reopen
/// sequence) otherwise. Always single-quoting unconditionally would also be
/// correct but produces noisy output for the common case of a plain value.
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

pub fn expand_tilde(s: &str, env: &Env) -> String {
    if s == "~" {
        return env.get("HOME").unwrap_or_else(|| "/".into());
    }
    if let Some(rest) = s.strip_prefix("~/") {
        let home = env.get("HOME").unwrap_or_else(|| "/".into());
        return format!("{home}/{rest}");
    }
    s.to_owned()
}

// ---------------------------------------------------------------------------
// Glob
// ---------------------------------------------------------------------------

pub fn glob_expand(pattern: &str) -> Vec<String> {
    let (dir, file_pat) = pattern
        .rfind('/')
        .map_or((".", pattern), |pos| (&pattern[..pos], &pattern[pos + 1..]));

    let Ok(entries) = std::fs::read_dir(dir) else {
        return vec![];
    };
    let mut results: Vec<String> = entries
        .filter_map(std::result::Result::ok)
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|name| {
            (file_pat.starts_with('.') || !name.starts_with('.')) && glob_match(file_pat, name)
        })
        .map(|name| {
            if dir == "." {
                name
            } else {
                format!("{dir}/{name}")
            }
        })
        .collect();
    results.sort();
    results
}

pub fn glob_match(pattern: &str, text: &str) -> bool {
    glob_inner(
        &pattern.chars().collect::<Vec<_>>(),
        &text.chars().collect::<Vec<_>>(),
    )
}

fn glob_inner(p: &[char], t: &[char]) -> bool {
    match p.first() {
        None => t.is_empty(),
        Some('*') => glob_inner(&p[1..], t) || (!t.is_empty() && glob_inner(p, &t[1..])),
        Some(pc) => {
            matches!(t.first(), Some(tc) if *pc == '?' || pc == tc) && glob_inner(&p[1..], &t[1..])
        }
    }
}

// ---------------------------------------------------------------------------
// Escape sequences (for echo -e and printf)
// ---------------------------------------------------------------------------

/// Consumes up to `max` leading ASCII hex digits from `chars`, stopping
/// early at the first non-hex character (bash's `\x`/`\u`/`\U` are all
/// non-greedy this way, e.g. `\u41 ` is just `A` followed by a space).
fn take_hex(chars: &mut std::iter::Peekable<std::str::Chars>, max: usize) -> String {
    let mut hex = String::new();
    while hex.len() < max && chars.peek().is_some_and(char::is_ascii_hexdigit) {
        hex.push(chars.next().unwrap());
    }
    hex
}

/// Expands bash's `echo -e`/`printf` backslash escapes. Returns the
/// expanded text and whether a `\c` was seen: that escape means "stop all
/// further output here", including any trailing newline `echo` would
/// otherwise add, so callers can't just treat it as a character to insert.
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
// ${var%pat}, ${var%%pat}, ${var:-word}, ${var:=word}, ${var:?msg}, ${var:+word}
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

pub fn parse_param_op(s: &str) -> Option<ParamOp<'_>> {
    // ${#var}: length, starts with '#' followed by a valid name or special param
    if let Some(var) = s.strip_prefix('#')
        && !var.is_empty()
    {
        return Some(ParamOp::Length(var));
    }

    // Find end of the variable name portion
    let name_end = s
        .char_indices()
        .take_while(|(_, c)| c.is_ascii_alphanumeric() || *c == '_')
        .last()
        .map_or(0, |(i, c)| i + c.len_utf8());

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
    for op in [":-", ":+", ":?", ":="] {
        if let Some(word) = rest.strip_prefix(op) {
            return Some(ParamOp::Conditional { var, op, word });
        }
    }

    None
}

pub fn strip_prefix(val: &str, pat: &str, greedy: bool) -> String {
    let chars: Vec<char> = val.chars().collect();
    let range: Box<dyn Iterator<Item = usize>> = if greedy {
        Box::new((0..=chars.len()).rev())
    } else {
        Box::new(0..=chars.len())
    };
    for len in range {
        let prefix: String = chars[..len].iter().collect();
        if glob_match(pat, &prefix) {
            return chars[len..].iter().collect();
        }
    }
    val.to_owned()
}

pub fn strip_suffix(val: &str, pat: &str, greedy: bool) -> String {
    let chars: Vec<char> = val.chars().collect();
    let range: Box<dyn Iterator<Item = usize>> = if greedy {
        Box::new(0..=chars.len())
    } else {
        Box::new((0..=chars.len()).rev())
    };
    for start in range {
        let suffix: String = chars[start..].iter().collect();
        if glob_match(pat, &suffix) {
            return chars[..start].iter().collect();
        }
    }
    val.to_owned()
}

// ---------------------------------------------------------------------------
// Arithmetic expansion: $(( expr ))
// Supports: integers, +,-,*,/,%, unary -/!, ==,!=,<,<=,>,>=, &&, ||, ()
// Variables are pre-substituted by the caller (Shell::expand_var).
// ---------------------------------------------------------------------------

pub fn eval_arith(expr: &str) -> i64 {
    let tokens = arith_tokenize(expr.trim());
    let mut pos = 0;
    arith_or(&tokens, &mut pos).unwrap_or(0)
}

#[derive(Clone)]
enum Tok {
    Num(i64),
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
    Bang,
    LParen,
    RParen,
}

fn arith_tokenize(s: &str) -> Vec<Tok> {
    let mut toks = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '+' => toks.push(Tok::Plus),
            '-' => toks.push(Tok::Minus),
            '*' => toks.push(Tok::Star),
            '/' => toks.push(Tok::Slash),
            '%' => toks.push(Tok::Percent),
            '(' => toks.push(Tok::LParen),
            ')' => toks.push(Tok::RParen),
            '!' if chars.peek() == Some(&'=') => {
                chars.next();
                toks.push(Tok::Ne);
            }
            '!' => toks.push(Tok::Bang),
            '=' if chars.peek() == Some(&'=') => {
                chars.next();
                toks.push(Tok::Eq);
            }
            '<' if chars.peek() == Some(&'=') => {
                chars.next();
                toks.push(Tok::Le);
            }
            '<' => toks.push(Tok::Lt),
            '>' if chars.peek() == Some(&'=') => {
                chars.next();
                toks.push(Tok::Ge);
            }
            '>' => toks.push(Tok::Gt),
            '&' if chars.peek() == Some(&'&') => {
                chars.next();
                toks.push(Tok::And);
            }
            '|' if chars.peek() == Some(&'|') => {
                chars.next();
                toks.push(Tok::Or);
            }
            '0'..='9' => {
                let mut n = c as i64 - '0' as i64;
                while matches!(chars.peek(), Some('0'..='9')) {
                    n = n * 10 + chars.next().unwrap() as i64 - '0' as i64;
                }
                toks.push(Tok::Num(n));
            }
            // Identifiers/unexpanded vars → 0
            'a'..='z' | 'A'..='Z' | '_' => {
                while matches!(chars.peek(), Some(ch) if ch.is_ascii_alphanumeric() || *ch == '_') {
                    chars.next();
                }
                toks.push(Tok::Num(0));
            }
            _ => {}
        }
    }
    toks
}

fn arith_or(t: &[Tok], p: &mut usize) -> Option<i64> {
    let mut v = arith_and(t, p)?;
    while matches!(t.get(*p), Some(Tok::Or)) {
        *p += 1;
        let r = arith_and(t, p)?;
        v = i64::from(v != 0 || r != 0);
    }
    Some(v)
}

fn arith_and(t: &[Tok], p: &mut usize) -> Option<i64> {
    let mut v = arith_cmp(t, p)?;
    while matches!(t.get(*p), Some(Tok::And)) {
        *p += 1;
        let r = arith_cmp(t, p)?;
        v = i64::from(v != 0 && r != 0);
    }
    Some(v)
}

fn arith_cmp(t: &[Tok], p: &mut usize) -> Option<i64> {
    let mut v = arith_add(t, p)?;
    loop {
        let op = match t.get(*p) {
            Some(Tok::Eq) => "==",
            Some(Tok::Ne) => "!=",
            Some(Tok::Lt) => "<",
            Some(Tok::Le) => "<=",
            Some(Tok::Gt) => ">",
            Some(Tok::Ge) => ">=",
            _ => break,
        };
        *p += 1;
        let r = arith_add(t, p)?;
        v = i64::from(match op {
            "==" => v == r,
            "!=" => v != r,
            "<" => v < r,
            "<=" => v <= r,
            ">" => v > r,
            ">=" => v >= r,
            _ => unreachable!(),
        });
    }
    Some(v)
}

fn arith_add(t: &[Tok], p: &mut usize) -> Option<i64> {
    let mut v = arith_mul(t, p)?;
    loop {
        match t.get(*p) {
            Some(Tok::Plus) => {
                *p += 1;
                v = v.wrapping_add(arith_mul(t, p)?);
            }
            Some(Tok::Minus) => {
                *p += 1;
                v = v.wrapping_sub(arith_mul(t, p)?);
            }
            _ => break,
        }
    }
    Some(v)
}

fn arith_mul(t: &[Tok], p: &mut usize) -> Option<i64> {
    let mut v = arith_unary(t, p)?;
    loop {
        match t.get(*p) {
            Some(Tok::Star) => {
                *p += 1;
                v = v.wrapping_mul(arith_unary(t, p)?);
            }
            Some(Tok::Slash) => {
                *p += 1;
                let r = arith_unary(t, p)?;
                v = if r == 0 { 0 } else { v.wrapping_div(r) };
            }
            Some(Tok::Percent) => {
                *p += 1;
                let r = arith_unary(t, p)?;
                v = if r == 0 { 0 } else { v.wrapping_rem(r) };
            }
            _ => break,
        }
    }
    Some(v)
}

fn arith_unary(t: &[Tok], p: &mut usize) -> Option<i64> {
    match t.get(*p) {
        Some(Tok::Minus) => {
            *p += 1;
            Some(arith_unary(t, p)?.wrapping_neg())
        }
        Some(Tok::Bang) => {
            *p += 1;
            Some(i64::from(arith_unary(t, p)? == 0))
        }
        Some(Tok::Plus) => {
            *p += 1;
            arith_unary(t, p)
        }
        _ => arith_primary(t, p),
    }
}

fn arith_primary(t: &[Tok], p: &mut usize) -> Option<i64> {
    match t.get(*p) {
        Some(Tok::Num(n)) => {
            let v = *n;
            *p += 1;
            Some(v)
        }
        Some(Tok::LParen) => {
            *p += 1;
            let v = arith_or(t, p)?;
            if matches!(t.get(*p), Some(Tok::RParen)) {
                *p += 1;
            }
            Some(v)
        }
        _ => None,
    }
}
