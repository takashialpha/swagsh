//! Brace expansion, as in bash: `a{b,c}d` is `abd acd`, `{1..5}` is
//! `1 2 3 4 5`, `{a..e..2}` is `a c e`.
//!
//! It depends only on a word's text, never on a variable's value, so the
//! parser does it, on the lexed text of each word: quoted text and
//! expansions (`${...}`, `$(...)`) are left alone, and what comes out is
//! parsed as words in the usual way.

use crate::lexer::{QUOTE_END, QUOTE_START};

/// The most words, and bytes in them, one word may expand to. Brace
/// expansion grows as the product of a word's brace expressions, so a short
/// word could otherwise exhaust memory.
const MAX_WORDS: usize = 1 << 20;
const MAX_BYTES: usize = 64 << 20;

/// The words that brace expansion makes of lexed word `raw`: `raw` itself
/// when it has no brace expression. `None` when there would be too many.
#[must_use]
pub fn expand(raw: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut bytes = 0;
    expand_into(raw, 0, &mut out, &mut bytes)?;
    Some(out)
}

/// Expands `raw`, whose first `from` bytes hold no brace expression.
fn expand_into(raw: &str, from: usize, out: &mut Vec<String>, bytes: &mut usize) -> Option<()> {
    let Some((open, close, items)) = first_brace(raw, from) else {
        *bytes += raw.len();
        if out.len() >= MAX_WORDS || *bytes > MAX_BYTES {
            return None;
        }
        out.push(raw.to_owned());
        return Some(());
    };
    let (pre, post) = (&raw[..open], &raw[close + 1..]);
    for item in items {
        expand_into(&format!("{pre}{item}{post}"), open, out, bytes)?;
    }
    Some(())
}

/// The first brace expression in `raw` at or after byte `from` that
/// expands: its `{` and `}` offsets, and the items it expands to.
fn first_brace(raw: &str, from: usize) -> Option<(usize, usize, Vec<String>)> {
    for (open, close) in brace_pairs(raw.as_bytes(), from) {
        let inner = &raw[open + 1..close];
        let commas = top_level_commas(inner);
        if !commas.is_empty() {
            let mut items = Vec::with_capacity(commas.len() + 1);
            let mut start = 0;
            for &c in &commas {
                items.push(inner[start..c].to_owned());
                start = c + 1;
            }
            items.push(inner[start..].to_owned());
            return Some((open, close, items));
        }
        if let Some(items) = sequence(inner) {
            return Some((open, close, items));
        }
    }
    None
}

/// Every unquoted `{` from `from` on with the `}` that closes it, in order
/// of the `{`, found in one pass.
fn brace_pairs(bytes: &[u8], from: usize) -> Vec<(usize, usize)> {
    let mut pairs = Vec::new();
    let mut open = Vec::new();
    let mut i = from;
    while i < bytes.len() {
        if let Some(next) = skip_opaque(bytes, i) {
            i = next;
            continue;
        }
        match bytes[i] {
            b'{' => open.push(i),
            b'}' => {
                if let Some(start) = open.pop() {
                    pairs.push((start, i));
                }
            }
            _ => {}
        }
        i += 1;
    }
    pairs.sort_unstable();
    pairs
}

/// Skips the quoted text or expansion starting at `bytes[i]`, returning
/// the offset just past it, or `None` if nothing starts there.
fn skip_opaque(bytes: &[u8], i: usize) -> Option<usize> {
    let mut j = i;
    match bytes[i] {
        b'\\' => Some((i + 2).min(bytes.len())),
        b if b == QUOTE_START as u8 => {
            while j < bytes.len() && bytes[j] != QUOTE_END as u8 {
                j += 1;
            }
            Some((j + 1).min(bytes.len()))
        }
        b'"' => {
            j += 1;
            while j < bytes.len() && bytes[j] != b'"' {
                j += if bytes[j] == b'\\' { 2 } else { 1 };
            }
            Some((j + 1).min(bytes.len()))
        }
        b'`' => {
            j += 1;
            while j < bytes.len() && bytes[j] != b'`' {
                j += if bytes[j] == b'\\' { 2 } else { 1 };
            }
            Some((j + 1).min(bytes.len()))
        }
        b'$' if matches!(bytes.get(i + 1), Some(b'{' | b'(')) => {
            let (open, close) = if bytes[i + 1] == b'{' {
                (b'{', b'}')
            } else {
                (b'(', b')')
            };
            let mut depth = 0usize;
            j = i + 1;
            while j < bytes.len() {
                match bytes[j] {
                    b if b == open => depth += 1,
                    b if b == close => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(j + 1);
                        }
                    }
                    b'\\' => j += 1,
                    _ => {}
                }
                j += 1;
            }
            Some(bytes.len())
        }
        _ => None,
    }
}

/// The offsets of the unquoted commas in `inner` outside nested braces.
fn top_level_commas(inner: &str) -> Vec<usize> {
    let bytes = inner.as_bytes();
    let mut commas = Vec::new();
    let mut depth = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        if let Some(next) = skip_opaque(bytes, i) {
            i = next;
            continue;
        }
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => commas.push(i),
            _ => {}
        }
        i += 1;
    }
    commas
}

/// `x..y` or `x..y..step`, of integers or of single letters.
fn sequence(inner: &str) -> Option<Vec<String>> {
    let mut parts = inner.split("..");
    let (from, to) = (parts.next()?, parts.next()?);
    let step = parts.next();
    if parts.next().is_some() {
        return None;
    }
    let step = match step {
        Some(s) => s.parse::<i64>().ok()?.unsigned_abs().max(1),
        None => 1,
    };
    if let (Ok(a), Ok(b)) = (from.parse::<i64>(), to.parse::<i64>()) {
        // `{01..10}`: as wide as the wider end, padded with zeros.
        let padded = |s: &str| {
            s.trim_start_matches('-').len() > 1 && s.trim_start_matches('-').starts_with('0')
        };
        let width = if padded(from) || padded(to) {
            from.len().max(to.len())
        } else {
            0
        };
        return Some(
            range(a, b, step)
                .take(MAX_WORDS + 1)
                .map(|n| format!("{n:0width$}"))
                .collect(),
        );
    }
    let (Some(a), Some(b)) = (single_letter(from), single_letter(to)) else {
        return None;
    };
    Some(
        range(i64::from(a), i64::from(b), step)
            .filter_map(|n| u8::try_from(n).ok().map(|c| char::from(c).to_string()))
            .collect(),
    )
}

const fn single_letter(s: &str) -> Option<u8> {
    match s.as_bytes() {
        [c] if c.is_ascii_alphabetic() => Some(*c),
        _ => None,
    }
}

/// From `a` to `b` inclusive, by `step`, in whichever direction.
fn range(a: i64, b: i64, step: u64) -> impl Iterator<Item = i64> {
    let step = i64::try_from(step).unwrap_or(i64::MAX);
    let mut next = Some(a);
    std::iter::from_fn(move || {
        let n = next?;
        next = if a <= b {
            n.checked_add(step).filter(|&m| m <= b)
        } else {
            n.checked_sub(step).filter(|&m| m >= b)
        };
        Some(n)
    })
}
