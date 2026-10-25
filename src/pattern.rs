//! Extended patterns (bash's `extglob`): `?(a|b)`, `*(a|b)`, `+(a|b)`,
//! `@(a|b)` and `!(a|b)`, alongside `*`, `?` and bracket expressions.
//!
//! Matching works out, for each part of a pattern, every text position it
//! can end at from a given start, rather than backtracking: `!(...)` needs
//! the full set anyway, and the work stays polynomial in the lengths.

use std::cell::Cell;

thread_local! {
    static ENABLED: Cell<bool> = const { Cell::new(true) };
}

/// Whether extended patterns are recognized (`shopt extglob`, on by
/// default).
#[must_use]
pub fn enabled() -> bool {
    ENABLED.get()
}

/// `shopt -s extglob` or `shopt -u extglob`.
pub fn set_enabled(on: bool) {
    ENABLED.set(on);
}

/// One part of a pattern.
enum Node {
    Char(char),
    /// `?`: any one character.
    Any,
    /// `*`: any run of characters.
    Star,
    /// A bracket expression, as the range of pattern characters it spans.
    Bracket(usize, usize),
    /// `op(alt|alt...)`, `op` being one of `? * + @ !`.
    Group(char, Vec<Vec<Self>>),
}

/// Whether `pattern` uses extended pattern syntax: one of `? * + @ !`
/// directly before an unescaped `(`.
#[must_use]
pub fn has_extglob(pattern: &str) -> bool {
    let mut prev = '\0';
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            chars.next();
            prev = '\0';
            continue;
        }
        if c == '(' && matches!(prev, '?' | '*' | '+' | '@' | '!') {
            return true;
        }
        prev = c;
    }
    false
}

/// Matches `text` against extended pattern `pattern`, ignoring case with
/// `nocase`. `None` if the pattern is not well formed (an unclosed group),
/// for the caller to fall back to matching it as an ordinary pattern.
#[must_use]
pub fn ext_match(pattern: &str, text: &str, nocase: bool) -> Option<bool> {
    let p: Vec<char> = pattern.chars().collect();
    let (nodes, end) = parse(&p, 0, false)?;
    if end != p.len() {
        return None;
    }
    let t: Vec<char> = text.chars().collect();
    let m = Matcher {
        p: &p,
        t: &t,
        nocase,
    };
    Some(m.ends(&nodes, 0)[t.len()])
}

/// Parses `p` from `i` until its end, or with `in_group`, until the `|` or
/// `)` that ends one alternative. Returns the nodes and where it stopped.
fn parse(p: &[char], mut i: usize, in_group: bool) -> Option<(Vec<Node>, usize)> {
    let mut nodes = Vec::new();
    while i < p.len() {
        let c = p[i];
        if in_group && (c == '|' || c == ')') {
            return Some((nodes, i));
        }
        if matches!(c, '?' | '*' | '+' | '@' | '!') && p.get(i + 1) == Some(&'(') {
            let mut alts = Vec::new();
            let mut j = i + 2;
            loop {
                let (alt, end) = parse(p, j, true)?;
                alts.push(alt);
                match p.get(end) {
                    Some('|') => j = end + 1,
                    Some(')') => {
                        j = end + 1;
                        break;
                    }
                    _ => return None,
                }
            }
            nodes.push(Node::Group(c, alts));
            i = j;
            continue;
        }
        match c {
            '\\' if i + 1 < p.len() => {
                nodes.push(Node::Char(p[i + 1]));
                i += 2;
            }
            '*' => {
                nodes.push(Node::Star);
                i += 1;
            }
            '?' => {
                nodes.push(Node::Any);
                i += 1;
            }
            '[' => {
                if let Some(end) = bracket_end(p, i) {
                    nodes.push(Node::Bracket(i, end));
                    i = end;
                } else {
                    nodes.push(Node::Char('['));
                    i += 1;
                }
            }
            _ => {
                nodes.push(Node::Char(c));
                i += 1;
            }
        }
    }
    (!in_group).then_some((nodes, i))
}

/// The index just past the `]` closing the bracket expression at `p[i]`.
fn bracket_end(p: &[char], i: usize) -> Option<usize> {
    let mut j = i + 1;
    if matches!(p.get(j), Some('!' | '^')) {
        j += 1;
    }
    if p.get(j) == Some(&']') {
        j += 1;
    }
    while j < p.len() {
        match p[j] {
            ']' => return Some(j + 1),
            '[' if p.get(j + 1) == Some(&':') => {
                let close = p[j + 2..].windows(2).position(|w| w == [':', ']'])?;
                j += close + 4;
            }
            '\\' => j += 2,
            _ => j += 1,
        }
    }
    None
}

struct Matcher<'a> {
    p: &'a [char],
    t: &'a [char],
    nocase: bool,
}

impl Matcher<'_> {
    fn same(&self, a: char, b: char) -> bool {
        a == b || (self.nocase && a.to_lowercase().eq(b.to_lowercase()))
    }

    /// Every position `seq` can end at, starting from `start`.
    fn ends(&self, seq: &[Node], start: usize) -> Vec<bool> {
        let mut at = vec![false; self.t.len() + 1];
        at[start] = true;
        for node in seq {
            let mut next = vec![false; self.t.len() + 1];
            for (pos, _) in at.iter().enumerate().filter(|(_, on)| **on) {
                self.step(node, pos, &mut next);
            }
            at = next;
            if !at.contains(&true) {
                break;
            }
        }
        at
    }

    /// Marks in `out` every position `node` can end at from `pos`.
    fn step(&self, node: &Node, pos: usize, out: &mut [bool]) {
        let len = self.t.len();
        match node {
            Node::Char(c) => {
                if pos < len && self.same(*c, self.t[pos]) {
                    out[pos + 1] = true;
                }
            }
            Node::Any => {
                if pos < len {
                    out[pos + 1] = true;
                }
            }
            Node::Star => out[pos..].fill(true),
            Node::Bracket(from, to) => {
                if pos < len && self.bracket(*from, *to, self.t[pos]) {
                    out[pos + 1] = true;
                }
            }
            Node::Group(op, alts) => self.group(*op, alts, pos, out),
        }
    }

    fn bracket(&self, from: usize, to: usize, c: char) -> bool {
        let matches = |c: char| crate::expand::bracket_matches(&self.p[from..to], c);
        matches(c)
            || (self.nocase && (c.to_lowercase().any(&matches) || c.to_uppercase().any(&matches)))
    }

    fn group(&self, op: char, alts: &[Vec<Node>], pos: usize, out: &mut [bool]) {
        let once = |from: usize| {
            let mut ends = vec![false; self.t.len() + 1];
            for alt in alts {
                for (i, on) in self.ends(alt, from).into_iter().enumerate() {
                    ends[i] |= on;
                }
            }
            ends
        };
        match op {
            '@' => merge(out, &once(pos)),
            '?' => {
                out[pos] = true;
                merge(out, &once(pos));
            }
            '*' | '+' => {
                // Repeat from every position reached until nothing new is.
                let mut reached = vec![false; self.t.len() + 1];
                let mut frontier = vec![pos];
                if op == '*' {
                    reached[pos] = true;
                }
                let mut seen = vec![false; self.t.len() + 1];
                while let Some(from) = frontier.pop() {
                    if std::mem::replace(&mut seen[from], true) {
                        continue;
                    }
                    for (i, on) in once(from).into_iter().enumerate() {
                        if on {
                            reached[i] = true;
                            // An empty repetition cannot get further.
                            if i != from {
                                frontier.push(i);
                            }
                        }
                    }
                }
                merge(out, &reached);
            }
            _ => {
                // `!(...)`: any run that no alternative matches whole.
                let matched = once(pos);
                for (i, slot) in out.iter_mut().enumerate().skip(pos) {
                    if !matched[i] {
                        *slot = true;
                    }
                }
            }
        }
    }
}

fn merge(out: &mut [bool], from: &[bool]) {
    for (o, f) in out.iter_mut().zip(from) {
        *o |= *f;
    }
}
