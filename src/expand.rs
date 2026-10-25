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

/// The `shopt` options that change pathname expansion.
#[derive(Debug, Clone, Copy, Default)]
pub struct GlobOptions {
    /// Names starting with `.` match without a leading `.` in the pattern
    /// (never `.` and `..` themselves).
    pub dotglob: bool,
    /// Matching ignores case.
    pub nocase: bool,
    /// A `**` component matches any depth of directories.
    pub globstar: bool,
}

/// Expands a pathname pattern against the filesystem, sorted in the
/// locale's collating sequence, which `LC_ALL`, `LC_COLLATE` and `LANG` pick.
///
/// Works one `/`-separated component at a time, so `a/*/b` and `/*/bin`
/// work, and returns nothing if there are no matches. A leading `.` in a
/// name is only matched by a component that itself starts with `.`, unless
/// `dotglob` is on.
#[must_use]
pub fn glob_expand(pattern: &str, opts: GlobOptions) -> Vec<String> {
    let components: Vec<&str> = pattern.split('/').filter(|c| !c.is_empty()).collect();
    let dirs_only = pattern.ends_with('/');
    let mut paths = vec![if pattern.starts_with('/') {
        "/".to_owned()
    } else {
        String::new()
    }];
    for (i, component) in components.iter().enumerate() {
        let last = i + 1 == components.len();
        let need_dir = dirs_only || !last;
        let mut next = Vec::new();
        for base in &paths {
            if opts.globstar && *component == "**" {
                globstar(base, !need_dir, opts, &mut next);
                continue;
            }
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
            let show_hidden =
                opts.dotglob || component.starts_with('.') || component.starts_with("\\.");
            for entry in entries.filter_map(Result::ok) {
                let name = entry.file_name().to_string_lossy().into_owned();
                if (show_hidden || !name.starts_with('.'))
                    && glob_match_case(component, &name, opts.nocase)
                {
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
    paths.retain(|p| !p.is_empty());
    if dirs_only {
        for path in &mut paths {
            path.push('/');
        }
    }
    crate::collate::sort(&mut paths);
    paths.dedup();
    paths
}

/// A `**` component under `base`: `base` and every directory below it,
/// and with `files`, as the last component, every file below it too.
/// Symbolic links to directories are not followed, as in bash.
fn globstar(base: &str, files: bool, opts: GlobOptions, out: &mut Vec<String>) {
    if !files {
        out.push(base.to_owned());
    }
    let dir = if base.is_empty() { "." } else { base };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') && !opts.dotglob {
            continue;
        }
        let path = join_path(base, &name);
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        if is_dir {
            if files {
                out.push(path.clone());
            }
            globstar(&path, files, opts, out);
        } else if files {
            out.push(path);
        }
    }
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
    if pattern.contains('(') && crate::pattern::enabled() && crate::pattern::has_extglob(pattern) {
        return true;
    }
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
    glob_match_case(pattern, text, false)
}

/// [`glob_match`], ignoring case with `nocase` (`nocasematch`,
/// `nocaseglob`). Extended patterns (`@(a|b)` and the like) are matched
/// unless `shopt -u extglob` turned them off.
#[must_use]
pub fn glob_match_case(pattern: &str, text: &str, nocase: bool) -> bool {
    // `(` first: it is in every extended pattern and almost no other.
    if pattern.contains('(')
        && crate::pattern::enabled()
        && crate::pattern::has_extglob(pattern)
        && let Some(matched) = crate::pattern::ext_match(pattern, text, nocase)
    {
        return matched;
    }
    if nocase {
        match_chars::<true>(pattern, text)
    } else {
        match_chars::<false>(pattern, text)
    }
}

/// The matcher behind [`glob_match_case`], compiled once per case rule so
/// the usual case-sensitive one stays plain comparisons.
fn match_chars<const NOCASE: bool>(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let same = |a: char, b: char| a == b || (NOCASE && a.to_lowercase().eq(b.to_lowercase()));

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
            if same(p[pi + 1], t[ti]) {
                pi += 2;
                ti += 1;
                continue;
            }
        } else if let Some((matched, next)) = (p.get(pi) == Some(&'['))
            .then(|| match_bracket_case(&p, pi, t[ti], NOCASE))
            .flatten()
        {
            if matched {
                pi = next;
                ti += 1;
                continue;
            }
        } else if pi < p.len() && (p[pi] == '?' || same(p[pi], t[ti])) {
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

/// [`match_bracket`], trying both cases of `c` with `nocase`.
fn match_bracket_case(p: &[char], start: usize, c: char, nocase: bool) -> Option<(bool, usize)> {
    let (matched, next) = match_bracket(p, start, c)?;
    if matched || !nocase {
        return Some((matched, next));
    }
    let other = c
        .to_lowercase()
        .chain(c.to_uppercase())
        .any(|o| o != c && match_bracket(p, start, o).is_some_and(|(m, _)| m));
    Some((other, next))
}

/// Whether bracket expression `p` (from its `[` to its `]`) matches `c`.
#[must_use]
pub fn bracket_matches(p: &[char], c: char) -> bool {
    match_bracket(p, 0, c).is_some_and(|(m, _)| m)
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

/// Decodes the inside of a `$'...'` string: the escapes of C, plus `\e`
/// and `\E` for escape and `\cX` for control-X.
#[must_use]
pub fn ansi_c_unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let Some(e) = chars.next() else {
            out.push('\\');
            break;
        };
        match e {
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            'a' => out.push('\x07'),
            'b' => out.push('\x08'),
            'e' | 'E' => out.push('\x1b'),
            'f' => out.push('\x0c'),
            'v' => out.push('\x0b'),
            '\\' | '\'' | '"' | '?' => out.push(e),
            'c' => match chars.next() {
                Some(x) => out.push(char::from(u8::try_from(u32::from(x) & 0x1f).unwrap_or(0))),
                None => out.push_str("\\c"),
            },
            '0'..='7' => {
                let mut value = e.to_digit(8).unwrap_or(0);
                for _ in 0..2 {
                    match chars.peek().and_then(|c| c.to_digit(8)) {
                        Some(d) => {
                            value = value * 8 + d;
                            chars.next();
                        }
                        None => break,
                    }
                }
                out.push(char::from_u32(value & 0xff).unwrap_or('\0'));
            }
            'x' | 'u' | 'U' => {
                let max = match e {
                    'x' => 2,
                    'u' => 4,
                    _ => 8,
                };
                let hex = take_hex(&mut chars, max);
                if hex.is_empty() {
                    out.push('\\');
                    out.push(e);
                } else {
                    let value = u32::from_str_radix(&hex, 16).unwrap_or(0);
                    out.push(char::from_u32(value).unwrap_or('\u{FFFD}'));
                }
            }
            other => {
                out.push('\\');
                out.push(other);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Parameter expansion: the text inside `${...}`
// ---------------------------------------------------------------------------

/// The parameter a `${...}` expansion names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Param<'a> {
    pub name: &'a str,
    /// The text between `[` and `]` after an array name: `@`, `*`, or an
    /// index or key still to be expanded.
    pub index: Option<&'a str>,
    /// `${!name}`: the parameter is the one `name`'s value names.
    pub indirect: bool,
}

impl Param<'_> {
    /// Expands to a list of values rather than one: `$@`, `$*`,
    /// `${a[@]}`, `${a[*]}`.
    #[must_use]
    pub fn is_list(&self) -> bool {
        matches!(self.name, "@" | "*") || matches!(self.index, Some("@" | "*"))
    }

    /// The list form joined into one field when quoted: `$*`, `${a[*]}`.
    #[must_use]
    pub fn is_star(&self) -> bool {
        self.name == "*" || self.index == Some("*")
    }
}

/// Where `${var/pattern/replacement}` replaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Anchor {
    /// `/`: the first match.
    First,
    /// `//`: every match.
    All,
    /// `/#`: a match at the start.
    Start,
    /// `/%`: a match at the end.
    End,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamOp<'a> {
    /// `${name}`, `${a[i]}`, `${!name}`: the value, with nothing done to it.
    Plain(Param<'a>),
    /// `${#name}`: the length in characters, or of a list, its count.
    Length(Param<'a>),
    /// `${!a[@]}`, `${!a[*]}`: an array's indices or keys.
    Keys(Param<'a>),
    /// `${!prefix@}`, `${!prefix*}`: the names of the variables starting
    /// with `prefix`.
    Names { prefix: &'a str, star: bool },
    PrefixStrip {
        var: Param<'a>,
        pat: &'a str,
        greedy: bool,
    },
    SuffixStrip {
        var: Param<'a>,
        pat: &'a str,
        greedy: bool,
    },
    Conditional {
        var: Param<'a>,
        op: &'a str,
        word: &'a str,
    },
    /// `${var:offset}`, `${var:offset:length}`: characters of a string, or
    /// elements of a list.
    Substring {
        var: Param<'a>,
        offset: &'a str,
        length: Option<&'a str>,
    },
    /// `${var/pattern/replacement}` and its `//`, `/#` and `/%` forms.
    Replace {
        var: Param<'a>,
        pat: &'a str,
        rep: Option<&'a str>,
        anchor: Anchor,
    },
    /// `${var@op}`: the value transformed: `Q` quoted for input, `E` with
    /// backslash escapes expanded, `P` expanded as a prompt, `A` as an
    /// assignment, `K`/`k` as key and value pairs, `a` its attributes, `U`,
    /// `u`, `L` uppercased, first character uppercased, lowercased.
    Transform { var: Param<'a>, op: char },
    /// `${var^pat}`, `${var^^pat}` (`upper`), `${var,pat}`, `${var,,pat}`:
    /// changes the case of the first character, or with `all`, every one,
    /// that matches `pat` (any character when empty).
    Case {
        var: Param<'a>,
        upper: bool,
        all: bool,
        pat: &'a str,
    },
}

impl ParamOp<'_> {
    /// The parameter the operator applies to, if one.
    #[must_use]
    pub const fn param(&self) -> Option<&Param<'_>> {
        match self {
            Self::Plain(p)
            | Self::Length(p)
            | Self::Keys(p)
            | Self::PrefixStrip { var: p, .. }
            | Self::SuffixStrip { var: p, .. }
            | Self::Conditional { var: p, .. }
            | Self::Substring { var: p, .. }
            | Self::Replace { var: p, .. }
            | Self::Transform { var: p, .. }
            | Self::Case { var: p, .. } => Some(p),
            Self::Names { .. } => None,
        }
    }

    /// Expands to separate fields inside double quotes, as `"$@"` does,
    /// and so to no field at all when empty.
    #[must_use]
    pub fn is_at_list(&self) -> bool {
        match self {
            Self::Length(_) => false,
            Self::Names { star, .. } => !star,
            op => op.param().is_some_and(|p| p.is_list() && !p.is_star()),
        }
    }
}

/// A variable name: a letter or `_`, then letters, digits and `_`.
#[must_use]
pub fn is_name(s: &str) -> bool {
    s.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Finds the first character at nesting depth zero for which `stop`
/// holds, skipping quoted text, backslash escapes and the insides of
/// `${...}`, `$(...)`, `(...)` and `[...]`. Returns its byte offset.
#[must_use]
pub fn find_unnested(s: &str, stop: impl Fn(char) -> bool) -> Option<usize> {
    let mut depth = 0usize;
    let mut chars = s.char_indices();
    let mut dquote = false;
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' => {
                chars.next();
            }
            crate::lexer::QUOTE_START => {
                for (_, c) in chars.by_ref() {
                    if c == crate::lexer::QUOTE_END {
                        break;
                    }
                }
            }
            '"' => dquote = !dquote,
            _ if depth == 0 && !dquote && stop(c) => return Some(i),
            '{' | '(' | '[' => depth += 1,
            '}' | ')' | ']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    None
}

/// Splits a parameter off the front of `s`: a name with an optional
/// subscript, a positional parameter's digits, or one special character.
fn split_param(s: &str) -> Option<(Param<'_>, &str)> {
    let first = s.chars().next()?;
    let name_end = if matches!(first, '@' | '*' | '#' | '?' | '-' | '$' | '!') {
        1
    } else if first.is_ascii_digit() {
        s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len())
    } else if first.is_ascii_alphabetic() || first == '_' {
        s.find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .unwrap_or(s.len())
    } else {
        return None;
    };
    let (name, rest) = s.split_at(name_end);
    let mut param = Param {
        name,
        index: None,
        indirect: false,
    };
    if is_name(name)
        && let Some(inner) = rest.strip_prefix('[')
    {
        let close = find_unnested(inner, |c| c == ']')?;
        if close == 0 {
            return None;
        }
        param.index = Some(&inner[..close]);
        return Some((param, &inner[close + 1..]));
    }
    Some((param, rest))
}

/// Parses the text of a `${...}` expansion. `None` for one that is not
/// valid: a bad substitution.
#[must_use]
pub fn parse_param_op(s: &str) -> Option<ParamOp<'_>> {
    // `${#}` is `$#`, `${#name}` its length; `${#-}` is the length of `$-`
    // but `${#-x}` tests `$#`.
    if let Some(rest) = s.strip_prefix('#')
        && let Some((param, "")) = split_param(rest)
    {
        return Some(ParamOp::Length(param));
    }
    let (indirect, body) = match s.strip_prefix('!') {
        Some(rest) if !rest.is_empty() => (true, rest),
        _ => (false, s),
    };
    if indirect {
        if let Some(prefix) = body.strip_suffix(['@', '*'])
            && is_name(prefix)
        {
            return Some(ParamOp::Names {
                prefix,
                star: body.ends_with('*'),
            });
        }
        if let Some((param, "")) = split_param(body)
            && param.index.is_some_and(|i| i == "@" || i == "*")
        {
            return Some(ParamOp::Keys(param));
        }
    }
    let (mut var, rest) = split_param(body)?;
    var.indirect = indirect;
    param_operator(var, rest)
}

/// The operator that follows `var` in a `${...}` expansion.
fn param_operator<'a>(var: Param<'a>, rest: &'a str) -> Option<ParamOp<'a>> {
    if rest.is_empty() {
        return Some(ParamOp::Plain(var));
    }
    if let Some(op) = rest.strip_prefix('@') {
        let mut ops = op.chars();
        return match (ops.next(), ops.next()) {
            (Some(op @ ('Q' | 'E' | 'P' | 'A' | 'K' | 'k' | 'a' | 'U' | 'u' | 'L')), None) => {
                Some(ParamOp::Transform { var, op })
            }
            _ => None,
        };
    }
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
    if let Some(spec) = rest.strip_prefix(':') {
        let (offset, length) = find_unnested(spec, |c| c == ':')
            .map_or((spec, None), |i| (&spec[..i], Some(&spec[i + 1..])));
        return Some(ParamOp::Substring {
            var,
            offset,
            length,
        });
    }
    if let Some(spec) = rest.strip_prefix('/') {
        let (anchor, spec) = match spec.as_bytes().first() {
            Some(b'/') => (Anchor::All, &spec[1..]),
            Some(b'#') => (Anchor::Start, &spec[1..]),
            Some(b'%') => (Anchor::End, &spec[1..]),
            _ => (Anchor::First, spec),
        };
        let (pat, rep) = find_unnested(spec, |c| c == '/')
            .map_or((spec, None), |i| (&spec[..i], Some(&spec[i + 1..])));
        return Some(ParamOp::Replace {
            var,
            pat,
            rep,
            anchor,
        });
    }
    for (op, upper, all) in [
        ("^^", true, true),
        ("^", true, false),
        (",,", false, true),
        (",", false, false),
    ] {
        if let Some(pat) = rest.strip_prefix(op) {
            return Some(ParamOp::Case {
                var,
                upper,
                all,
                pat,
            });
        }
    }
    None
}

/// The characters of `val` from `offset` for `length`, by bash's rules: a
/// negative offset counts from the end, and a negative length leaves that
/// many characters off the end.
#[must_use]
pub fn substring(val: &str, offset: i64, length: Option<i64>) -> String {
    let chars: Vec<char> = val.chars().collect();
    slice_range(chars.len(), offset, length)
        .map(|r| chars[r].iter().collect())
        .unwrap_or_default()
}

/// The index range `offset`/`length` select from `len` items, as for
/// [`substring`]; `None` when it selects nothing.
#[must_use]
pub fn slice_range(len: usize, offset: i64, length: Option<i64>) -> Option<std::ops::Range<usize>> {
    let len_i = i64::try_from(len).ok()?;
    let start = if offset < 0 { len_i + offset } else { offset };
    if start < 0 || start > len_i {
        return None;
    }
    let end = match length {
        None => len_i,
        Some(n) if n < 0 => len_i + n,
        Some(n) => start.saturating_add(n).min(len_i),
    };
    if end < start {
        return None;
    }
    Some(usize::try_from(start).ok()?..usize::try_from(end).ok()?)
}

/// `${val/pat/rep}` and its other forms: replaces the longest match of
/// `pat` at the place `anchor` says. `rep` is called with the matched text
/// (bash puts it where the replacement has an unquoted `&`).
pub fn replace_matches(
    val: &str,
    pat: &str,
    anchor: Anchor,
    mut rep: impl FnMut(&str) -> String,
) -> String {
    if pat.is_empty() && !matches!(anchor, Anchor::Start | Anchor::End) {
        return val.to_owned();
    }
    let bounds: Vec<usize> = char_boundaries(val).collect();
    let matches = |start: usize, end: usize| glob_match(pat, &val[start..end]);
    match anchor {
        Anchor::Start => {
            for &end in bounds.iter().rev() {
                if matches(0, end) {
                    return format!("{}{}", rep(&val[..end]), &val[end..]);
                }
            }
            val.to_owned()
        }
        Anchor::End => {
            for &start in &bounds {
                if matches(start, val.len()) {
                    return format!("{}{}", &val[..start], rep(&val[start..]));
                }
            }
            val.to_owned()
        }
        Anchor::First | Anchor::All => {
            let mut out = String::with_capacity(val.len());
            let mut i = 0;
            let mut copied = 0;
            while i < bounds.len() {
                let start = bounds[i];
                let found = bounds[i + 1..]
                    .iter()
                    .rev()
                    .copied()
                    .find(|&end| matches(start, end));
                if let Some(end) = found {
                    out.push_str(&val[copied..start]);
                    out.push_str(&rep(&val[start..end]));
                    copied = end;
                    if anchor == Anchor::First {
                        break;
                    }
                    i = bounds.partition_point(|&b| b < end);
                } else {
                    i += 1;
                }
            }
            out.push_str(&val[copied..]);
            out
        }
    }
}

/// `${val^pat}` and friends; see [`ParamOp::Case`].
#[must_use]
pub fn change_case(val: &str, upper: bool, all: bool, pat: &str) -> String {
    let mut out = String::with_capacity(val.len());
    let mut buf = [0u8; 4];
    for (i, c) in val.chars().enumerate() {
        let applies =
            (all || i == 0) && (pat.is_empty() || glob_match(pat, c.encode_utf8(&mut buf)));
        if !applies {
            out.push(c);
        } else if upper {
            out.extend(c.to_uppercase());
        } else {
            out.extend(c.to_lowercase());
        }
    }
    out
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
// C-style integer arithmetic on `i64`: decimal, octal (`010`) and hex
// (`0x10`) constants, variables and array elements by name, and every C
// operator but `sizeof`, `++`, `--` and `,` included. Overflow wraps. `$`
// references have already been expanded by the caller.
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
    /// Element `index` of array `name`: `a[i]`. `index` is the subscript's
    /// text; it is an expression unless `name` is an associative array.
    ///
    /// # Errors
    ///
    /// Returns an error if `index` is not a valid subscript.
    fn get_elem(&mut self, name: &str, index: &str) -> Result<Option<String>, ArithError> {
        let _ = (name, index);
        Ok(None)
    }
    /// Assigns element `index` of array `name`.
    ///
    /// # Errors
    ///
    /// Returns an error if `index` is not a valid subscript or `name`
    /// cannot be assigned.
    fn set_elem(&mut self, name: &str, index: &str, value: i64) -> Result<(), ArithError> {
        let _ = (name, index, value);
        Err(ArithError("arrays are not available here".to_owned()))
    }
}

impl Env {
    /// The element of `name` that arithmetic subscript `index` names.
    fn arith_key(&mut self, name: &str, index: &str) -> Result<crate::env::Key, ArithError> {
        if self.is_assoc(name) {
            return Ok(crate::env::Key::Name(index.trim().to_owned()));
        }
        let i = eval_arith_with(index, self)?;
        self.resolve_index(name, i)
            .map(crate::env::Key::Index)
            .ok_or_else(|| ArithError(format!("{name}[{index}]: bad array subscript")))
    }
}

impl ArithVars for Env {
    fn get_var(&self, name: &str) -> Option<&str> {
        self.get_ref(name)
    }
    fn set_var(&mut self, name: &str, value: i64) -> Result<(), ArithError> {
        self.assign(name, value.to_string())
            .map_err(|e| ArithError(e.to_string()))
    }
    fn get_elem(&mut self, name: &str, index: &str) -> Result<Option<String>, ArithError> {
        let key = self.arith_key(name, index)?;
        Ok(self.element(name, &key).map(str::to_owned))
    }
    fn set_elem(&mut self, name: &str, index: &str, value: i64) -> Result<(), ArithError> {
        let key = self.arith_key(name, index)?;
        self.assign_element(name, key, value.to_string(), false)
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
    let value = parser.comma(true)?;
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
    /// `name[index]`, with the subscript's text.
    Elem(&'a str, &'a str),
    /// `++` and `--`, before or after a variable.
    Inc,
    Dec,
    Comma,
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
                toks.push(arith_name(s, start, &mut chars)?);
                continue;
            }
            // `++`/`--` only next to a variable; elsewhere, as in `5--3`,
            // they are two signs.
            '+' | '-'
                if chars.peek().is_some_and(|&(_, n)| n == c)
                    && (matches!(toks.last(), Some(Tok::Ident(_) | Tok::Elem(..)))
                        || s[start + 2..]
                            .trim_start()
                            .starts_with(|n: char| n.is_ascii_alphabetic() || n == '_')) =>
            {
                chars.next();
                toks.push(if c == '+' { Tok::Inc } else { Tok::Dec });
                continue;
            }
            ',' => Tok::Comma,
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

/// A variable, or with a subscript, an array element, starting at byte
/// `start` of `s`.
fn arith_name<'a>(
    s: &'a str,
    start: usize,
    chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>,
) -> Result<Tok<'a>, ArithError> {
    let end = take_while_end(chars, s.len(), |c| c.is_ascii_alphanumeric() || c == '_');
    let name = &s[start..end];
    let Some(inner) = s[end..].strip_prefix('[') else {
        return Ok(Tok::Ident(name));
    };
    let close = find_unnested(inner, |c| c == ']')
        .ok_or_else(|| ArithError(format!("{name}: unterminated subscript")))?;
    while chars.next_if(|&(i, _)| i <= end + 1 + close).is_some() {}
    Ok(Tok::Elem(name, &inner[..close]))
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
    parse_arith_value(name, vars.get_var(name))
}

fn parse_arith_value(name: &str, value: Option<&str>) -> Result<i64, ArithError> {
    match value {
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

    /// `expression := assignment (',' assignment)*`: the last one's value.
    fn comma(&mut self, eval: bool) -> Result<i64, ArithError> {
        let mut value = self.assign(eval)?;
        while self.eat(Tok::Comma) {
            value = self.assign(eval)?;
        }
        Ok(value)
    }

    /// `assignment := lvalue ('=' | 'op=') assignment | conditional`
    fn assign(&mut self, eval: bool) -> Result<i64, ArithError> {
        self.nested(|p| {
            let (Some(target @ (Tok::Ident(_) | Tok::Elem(..))), Some(Tok::Assign(op))) =
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
                Some(op) => op.apply(p.read(target)?, rhs)?,
                None => rhs,
            };
            p.write(target, value)?;
            Ok(value)
        })
    }

    /// The value of variable or element `target`.
    fn read(&mut self, target: Tok<'_>) -> Result<i64, ArithError> {
        match target {
            Tok::Ident(name) => read_arith_var(self.vars, name),
            Tok::Elem(name, index) => {
                let value = self.vars.get_elem(name, index)?;
                parse_arith_value(&format!("{name}[{index}]"), value.as_deref())
            }
            _ => Err(ArithError("expected a variable".to_owned())),
        }
    }

    fn write(&mut self, target: Tok<'_>, value: i64) -> Result<(), ArithError> {
        match target {
            Tok::Ident(name) => self.vars.set_var(name, value),
            Tok::Elem(name, index) => self.vars.set_elem(name, index, value),
            _ => Err(ArithError("expected a variable".to_owned())),
        }
    }

    /// `++target` or `--target` (`pre`), or `target++`/`target--`.
    fn step(&mut self, target: Tok<'_>, by: i64, pre: bool, eval: bool) -> Result<i64, ArithError> {
        if !eval {
            return Ok(0);
        }
        let old = self.read(target)?;
        let new = old.wrapping_add(by);
        self.write(target, new)?;
        Ok(if pre { new } else { old })
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
        if let Some(step @ (Tok::Inc | Tok::Dec)) = self.peek() {
            let target = self.tokens.get(self.pos + 1).copied();
            let Some(target @ (Tok::Ident(_) | Tok::Elem(..))) = target else {
                return Err(ArithError("`++`/`--` needs a variable".to_owned()));
            };
            self.pos += 2;
            return self.step(target, if step == Tok::Inc { 1 } else { -1 }, true, eval);
        }
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
            Some(target @ (Tok::Ident(_) | Tok::Elem(..))) => {
                self.pos += 1;
                if let Some(step @ (Tok::Inc | Tok::Dec)) = self.peek() {
                    self.pos += 1;
                    return self.step(target, if step == Tok::Inc { 1 } else { -1 }, false, eval);
                }
                return if eval { self.read(target) } else { Ok(0) };
            }
            Some(Tok::LParen) => {
                self.pos += 1;
                let inner = self.comma(eval)?;
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
