//! Syntax highlighting of the line being typed.
//!
//! The line is usually incomplete, so rather than the parser this uses a
//! forgiving scan of its own: it finds words, quotes, expansions,
//! operators and comments, and which words are in command position. A
//! command is colored by whether it exists, and an argument naming a file
//! that exists is underlined, as fish does.

use std::path::Path;

/// What a stretch of the line is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Command,
    Unknown,
    Keyword,
    String,
    Variable,
    Operator,
    Option,
    Comment,
    Path,
    Error,
    Bracket,
}

/// A stretch of the line, by byte offsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub kind: Kind,
}

/// What the scan needs to know about the shell: which commands exist.
pub trait Lookup {
    fn command_exists(&self, name: &str) -> bool;
    /// The home directory, for `~/` in paths.
    fn home(&self) -> Option<&str>;
}

/// The reserved words after which another command starts.
const COMMAND_STARTERS: &[&str] = &[
    "if", "then", "else", "elif", "while", "until", "do", "{", "!",
];

/// The commands that run the command after them, with their own options
/// first: `exec -a name cmd`, `sudo -E cmd`.
const PREFIX_COMMANDS: &[&str] = &[
    "time", "exec", "command", "builtin", "sudo", "doas", "nohup", "env", "xargs", "nice",
    "strace", "watch",
];

/// The reserved words.
pub const KEYWORDS: &[&str] = &[
    "!", "[[", "]]", "{", "}", "case", "coproc", "do", "done", "elif", "else", "esac", "fi", "for",
    "function", "if", "in", "select", "then", "time", "until", "while",
];

/// The result of scanning a line.
#[derive(Debug, Default)]
pub struct Scan {
    pub spans: Vec<Span>,
    /// Where each word starts and ends, and whether it is in command
    /// position.
    pub words: Vec<(usize, usize, bool)>,
}

/// Scans `line` for highlighting.
pub fn scan(line: &str, lookup: &dyn Lookup) -> Scan {
    let mut scan = Scan::default();
    Scanner {
        s: line.as_bytes(),
        line,
        lookup,
        out: &mut scan,
        pending_case: false,
        heredocs: Vec::new(),
    }
    .run(0, line.len());
    scan
}

struct Scanner<'a> {
    s: &'a [u8],
    line: &'a str,
    lookup: &'a dyn Lookup,
    out: &'a mut Scan,
    /// A `case` was read, whose `in` starts its patterns.
    pending_case: bool,
    /// The here-documents whose bodies start on the next line: each
    /// delimiter, and whether its tabs are stripped (`<<-`).
    heredocs: Vec<(String, bool)>,
}

impl Scanner<'_> {
    fn span(&mut self, start: usize, end: usize, kind: Kind) {
        if start < end {
            self.out.spans.push(Span { start, end, kind });
        }
    }

    /// Scans the commands in `start..end`.
    fn run(&mut self, start: usize, end: usize) {
        let mut i = start;
        let mut command_position = true;
        // After `for`, `select` or `case`, until `in`: the words are not
        // commands.
        let mut before_in = false;
        // In the patterns of a `case`: a `)` starts the commands, and `;;`
        // goes back to patterns.
        let mut case_patterns = false;
        // After a command such as `sudo`, whose options come before the
        // command it runs.
        let mut after_prefix = false;
        while i < end {
            let b = self.s[i];
            match b {
                b' ' | b'\t' => i += 1,
                b'\n' => {
                    i = self.heredoc_bodies(i + 1, end);
                    command_position = true;
                    before_in = false;
                    after_prefix = false;
                }
                b'#' => {
                    let stop = self.line[i..end].find('\n').map_or(end, |n| i + n);
                    self.span(i, stop, Kind::Comment);
                    i = stop;
                }
                b'(' if command_position && self.s[i..end].starts_with(b"((") => {
                    // `(( expression ))`: arithmetic, not commands.
                    let close = self.line[i + 2..end].find("))").map(|n| i + 2 + n);
                    self.span(i, i + 2, Kind::Operator);
                    if let Some(c) = close {
                        self.span(c, c + 2, Kind::Operator);
                        i = c + 2;
                    } else {
                        self.span(i, i + 2, Kind::Error);
                        i = end;
                    }
                    command_position = false;
                }
                b'|' | b'&' | b';' | b'(' | b')' => {
                    let len = operator_len(&self.s[i..end]);
                    let op = &self.s[i..i + len];
                    self.span(i, i + len, Kind::Operator);
                    command_position = if case_patterns && op == b")" {
                        case_patterns = false;
                        true
                    } else if matches!(op, b";;" | b";&" | b";;&") {
                        case_patterns = true;
                        false
                    } else {
                        b != b')'
                    };
                    before_in = false;
                    after_prefix = false;
                    i += len;
                }
                b'<' | b'>' => {
                    let len = operator_len(&self.s[i..end]);
                    let op = &self.s[i..i + len];
                    let heredoc = matches!(op, b"<<" | b"<<-").then_some(op == b"<<-");
                    self.span(i, i + len, Kind::Operator);
                    i += len;
                    // The redirection's target is not a command.
                    while i < end && matches!(self.s[i], b' ' | b'\t') {
                        i += 1;
                    }
                    let word_end = self.word(i, end, false);
                    if let Some(strip_tabs) = heredoc
                        && word_end > i
                    {
                        let delimiter = self.line[i..word_end].replace(['\'', '"', '\\'], "");
                        self.heredocs.push((delimiter, strip_tabs));
                    }
                    i = word_end.max(i + 1).min(end);
                }
                _ => {
                    let prefix_option = after_prefix && b == b'-';
                    let command =
                        command_position && !before_in && !case_patterns && !prefix_option;
                    let word_end = self.word(i, end, command);
                    if prefix_option {
                        i = word_end.max(i + 1);
                        continue;
                    }
                    let word = &self.line[i..word_end];
                    if before_in {
                        if word == "in" {
                            self.span(i, word_end, Kind::Keyword);
                            before_in = false;
                            case_patterns = self.pending_case;
                            self.pending_case = false;
                        }
                    } else if case_patterns {
                        if word == "esac" {
                            self.span(i, word_end, Kind::Keyword);
                            case_patterns = false;
                            command_position = false;
                        }
                    } else if command && !is_assignment(word) {
                        after_prefix = PREFIX_COMMANDS.contains(&word);
                        command_position = after_prefix || COMMAND_STARTERS.contains(&word);
                        before_in = matches!(word, "for" | "select" | "case");
                        self.pending_case = word == "case";
                    }
                    i = word_end.max(i + 1);
                }
            }
        }
    }

    /// Marks the bodies of the here-documents started on the line before
    /// `start`, returning where the line after the last one starts.
    fn heredoc_bodies(&mut self, start: usize, end: usize) -> usize {
        let mut i = start;
        for (delimiter, strip_tabs) in std::mem::take(&mut self.heredocs) {
            let body = i;
            loop {
                if i >= end {
                    self.span(body, end, Kind::String);
                    return end;
                }
                let stop = self.line[i..end].find('\n').map_or(end, |n| i + n);
                let text = &self.line[i..stop];
                let text = if strip_tabs {
                    text.trim_start_matches('\t')
                } else {
                    text
                };
                if text == delimiter {
                    self.span(body, i.saturating_sub(1).max(body), Kind::String);
                    self.span(stop - text.len(), stop, Kind::Operator);
                    i = (stop + 1).min(end);
                    break;
                }
                i = stop + 1;
            }
        }
        i
    }

    /// Scans the word starting at `start`, returning where it ends.
    fn word(&mut self, start: usize, end: usize, command: bool) -> usize {
        let mut i = start;
        let mut plain = true;
        // Open groups of extended patterns, `@(a|b)`, whose `(`, `)` and `|`
        // are the word's.
        let mut groups = 0usize;
        let spans_before = self.out.spans.len();
        while i < end {
            match self.s[i] {
                b'(' if i > start && b"@?*+!".contains(&self.s[i - 1]) => {
                    groups += 1;
                    i += 1;
                }
                b')' | b'|' if groups > 0 => {
                    groups -= usize::from(self.s[i] == b')');
                    i += 1;
                }
                b' ' | b'\t' | b'\n' | b'|' | b'&' | b';' | b'<' | b'>' | b'(' | b')' => break,
                b'\\' => i = (i + 2).min(end),
                b'\'' => {
                    plain = false;
                    let close = self.s[i + 1..end].iter().position(|&c| c == b'\'');
                    let next = close.map_or(end, |n| i + 1 + n + 1);
                    self.span(
                        i,
                        next,
                        if close.is_some() {
                            Kind::String
                        } else {
                            Kind::Error
                        },
                    );
                    i = next;
                }
                b'"' => {
                    plain = false;
                    i = self.double_quoted(i, end);
                }
                b'$' => {
                    plain = false;
                    i = self.dollar(i, end);
                }
                b'`' => {
                    plain = false;
                    let close = self.s[i + 1..end].iter().position(|&c| c == b'`');
                    let next = close.map_or(end, |n| i + 1 + n + 1);
                    self.span(i, i + 1, Kind::Operator);
                    self.run(i + 1, next.saturating_sub(1).max(i + 1));
                    if close.is_some() {
                        self.span(next - 1, next, Kind::Operator);
                    } else {
                        self.span(i, i + 1, Kind::Error);
                    }
                    i = next;
                }
                _ => i += 1,
            }
        }
        let text = &self.line[start..i];
        self.out.words.push((start, i, command));
        let kind = if command {
            if KEYWORDS.contains(&text) {
                Some(Kind::Keyword)
            } else if is_assignment(text) {
                let name_end = start + text.find('=').map_or(0, |n| n + 1);
                self.span(start, name_end, Kind::Variable);
                None
            } else if plain && !text.is_empty() {
                Some(if self.lookup.command_exists(&unescape(text)) {
                    Kind::Command
                } else {
                    Kind::Unknown
                })
            } else {
                None
            }
        } else if text == "]]" {
            Some(Kind::Keyword)
        } else if text.starts_with('-') && plain {
            Some(Kind::Option)
        } else if plain && !text.is_empty() && self.names_file(text) {
            Some(Kind::Path)
        } else {
            None
        };
        if let Some(kind) = kind {
            // Under the parts already found inside the word.
            self.out.spans.insert(
                spans_before,
                Span {
                    start,
                    end: i,
                    kind,
                },
            );
        }
        i
    }

    /// Scans the `"..."` starting at `start`, returning where it ends.
    fn double_quoted(&mut self, start: usize, end: usize) -> usize {
        let mut i = start + 1;
        let mut from = start;
        loop {
            if i >= end {
                self.span(from, end, Kind::String);
                self.span(start, start + 1, Kind::Error);
                return end;
            }
            match self.s[i] {
                b'"' => {
                    self.span(from, i + 1, Kind::String);
                    return i + 1;
                }
                b'\\' => i += 2,
                b'$' => {
                    self.span(from, i, Kind::String);
                    i = self.dollar(i, end);
                    from = i;
                }
                _ => i += 1,
            }
        }
    }

    /// Scans the expansion starting with the `$` at `start`, returning
    /// where it ends.
    fn dollar(&mut self, start: usize, end: usize) -> usize {
        let rest = &self.s[start + 1..end];
        match rest.first() {
            Some(b'(') => {
                let arith = rest.get(1) == Some(&b'(');
                let open = if arith { 3 } else { 2 };
                let close = matching(&self.s[..end], start + 1);
                self.span(start, start + open, Kind::Operator);
                let inner_end = close.unwrap_or(end);
                if arith {
                    self.span(
                        start + open,
                        inner_end.saturating_sub(1).max(start + open),
                        Kind::Variable,
                    );
                } else {
                    self.run(start + open, inner_end);
                }
                if let Some(c) = close {
                    let from = if arith { c - 1 } else { c };
                    self.span(from, c + 1, Kind::Operator);
                    c + 1
                } else {
                    self.span(start, start + open, Kind::Error);
                    end
                }
            }
            Some(b'{') => {
                let close = self.s[start..end].iter().position(|&c| c == b'}');
                let next = close.map_or(end, |n| start + n + 1);
                self.span(
                    start,
                    next,
                    if close.is_some() {
                        Kind::Variable
                    } else {
                        Kind::Error
                    },
                );
                next
            }
            Some(b'\'') => {
                let close = rest[1..].iter().position(|&c| c == b'\'');
                let next = close.map_or(end, |n| start + 2 + n + 1);
                self.span(
                    start,
                    next,
                    if close.is_some() {
                        Kind::String
                    } else {
                        Kind::Error
                    },
                );
                next
            }
            Some(c) if c.is_ascii_alphabetic() || *c == b'_' => {
                let len = rest
                    .iter()
                    .position(|c| !(c.is_ascii_alphanumeric() || *c == b'_'))
                    .unwrap_or(rest.len());
                self.span(start, start + 1 + len, Kind::Variable);
                start + 1 + len
            }
            Some(c) if c.is_ascii_digit() || b"@*#?-$!".contains(c) => {
                self.span(start, start + 2, Kind::Variable);
                start + 2
            }
            _ => start + 1,
        }
    }

    /// Whether plain word `text` names a file or directory that exists.
    fn names_file(&self, text: &str) -> bool {
        let text = unescape(text);
        if text.contains(['*', '?', '[']) {
            return false;
        }
        let path = match (text.strip_prefix("~/"), self.lookup.home()) {
            (Some(rest), Some(home)) => Path::new(home).join(rest),
            _ if text == "~" => return self.lookup.home().is_some(),
            _ => Path::new(&text).to_path_buf(),
        };
        path.symlink_metadata().is_ok()
    }
}

/// The length of the operator at the start of `s`.
fn operator_len(s: &[u8]) -> usize {
    const OPERATORS: &[&[u8]] = &[
        b";;&", b"<<<", b"<<-", b"&>>", b"||", b"&&", b";;", b";&", b"|&", b">>", b"<<", b">&",
        b"<&", b"<>", b">|", b"&>", b"((", b"))",
    ];
    OPERATORS
        .iter()
        .find(|op| s.starts_with(op))
        .map_or(1, |op| op.len())
}

/// Whether `word` is `NAME=...` or `NAME+=...`.
fn is_assignment(word: &str) -> bool {
    word.find('=').is_some_and(|eq| {
        let name = word[..eq].strip_suffix('+').unwrap_or(&word[..eq]);
        let name = name.split('[').next().unwrap_or(name);
        crate::expand::is_name(name)
    })
}

/// `word` without its backslash escapes.
fn unescape(word: &str) -> String {
    let mut out = String::with_capacity(word.len());
    let mut chars = word.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            out.extend(chars.next());
        } else {
            out.push(c);
        }
    }
    out
}

/// The offset of the bracket matching the one at `at`, if any.
#[must_use]
pub fn matching(s: &[u8], at: usize) -> Option<usize> {
    let (open, close, forward) = match s.get(at)? {
        b'(' => (b'(', b')', true),
        b'[' => (b'[', b']', true),
        b'{' => (b'{', b'}', true),
        b')' => (b'(', b')', false),
        b']' => (b'[', b']', false),
        b'}' => (b'{', b'}', false),
        _ => return None,
    };
    let mut depth = 0usize;
    if forward {
        for (i, &c) in s.iter().enumerate().skip(at) {
            if c == open {
                depth += 1;
            } else if c == close {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
        }
    } else {
        for i in (0..=at).rev() {
            if s[i] == close {
                depth += 1;
            } else if s[i] == open {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
        }
    }
    None
}

/// The styles to draw each kind in, as ANSI sequences.
pub struct Styles {
    pub styles: [String; 11],
    pub suggestion: String,
}

impl Styles {
    fn of(&self, kind: Kind) -> &str {
        &self.styles[kind as usize]
    }
}

/// `line` from `from` on, with the spans of `scan` drawn in `styles`, and
/// the bracket matching the one at or before the cursor `pos` marked. What
/// comes before `from` is the lines already read of the same command.
#[must_use]
pub fn render(line: &str, from: usize, pos: usize, scan: &Scan, styles: &Styles) -> String {
    let mut kinds: Vec<Option<Kind>> = vec![None; line.len()];
    for span in &scan.spans {
        for k in &mut kinds[span.start..span.end.min(line.len())] {
            *k = Some(span.kind);
        }
    }
    let bytes = line.as_bytes();
    let bracket_at = [pos, pos.wrapping_sub(1)]
        .into_iter()
        .find(|&p| p < bytes.len() && b"()[]{}".contains(&bytes[p]));
    if let Some(at) = bracket_at
        && let Some(other) = matching(bytes, at)
    {
        kinds[other] = Some(Kind::Bracket);
    }
    let mut out = String::with_capacity((line.len() - from) * 2);
    let mut current: Option<Kind> = None;
    for (i, c) in line[from..].char_indices().map(|(i, c)| (from + i, c)) {
        let kind = kinds[i];
        if kind != current {
            if current.is_some() {
                out.push_str("\x1b[0m");
            }
            if let Some(k) = kind {
                out.push_str(styles.of(k));
            }
            current = kind;
        }
        out.push(c);
    }
    if current.is_some() {
        out.push_str("\x1b[0m");
    }
    out
}
