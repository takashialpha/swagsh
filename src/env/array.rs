//! Array variables: indexed arrays (`a=(x y)`, `a[3]=z`), which may have
//! gaps, and associative ones (`declare -A m; m[key]=v`).

use std::collections::BTreeMap;
use std::fmt::Write as _;

/// An array variable's elements.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Array {
    /// Elements by index, in index order; unset indices are simply absent.
    Indexed(BTreeMap<usize, String>),
    /// Elements by key, in key order (as ksh93 lists them; bash's order is
    /// that of its hash table).
    Assoc(BTreeMap<String, String>),
}

/// Which element of an array: an index already resolved to a position, or
/// an associative array's key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Index(usize),
    Name(String),
}

impl Array {
    #[must_use]
    pub const fn new(assoc: bool) -> Self {
        if assoc {
            Self::Assoc(BTreeMap::new())
        } else {
            Self::Indexed(BTreeMap::new())
        }
    }

    #[must_use]
    pub const fn is_assoc(&self) -> bool {
        matches!(self, Self::Assoc(_))
    }

    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Indexed(m) => m.len(),
            Self::Assoc(m) => m.len(),
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[must_use]
    pub fn get(&self, key: &Key) -> Option<&str> {
        match (self, key) {
            (Self::Indexed(m), Key::Index(i)) => m.get(i),
            (Self::Assoc(m), Key::Name(k)) => m.get(k),
            (Self::Indexed(m), Key::Name(k)) => k.parse().ok().and_then(|i| m.get(&i)),
            (Self::Assoc(m), Key::Index(i)) => m.get(&i.to_string()),
        }
        .map(String::as_str)
    }

    /// The element that a plain `$name` reads and `name=value` writes:
    /// index 0, or key `0`.
    #[must_use]
    pub fn first_key(&self) -> Key {
        match self {
            Self::Indexed(_) => Key::Index(0),
            Self::Assoc(_) => Key::Name("0".to_owned()),
        }
    }

    pub fn set(&mut self, key: Key, value: String) {
        match (self, key) {
            (Self::Indexed(m), Key::Index(i)) => {
                m.insert(i, value);
            }
            (Self::Assoc(m), Key::Name(k)) => {
                m.insert(k, value);
            }
            (Self::Indexed(m), Key::Name(k)) => {
                if let Ok(i) = k.parse() {
                    m.insert(i, value);
                }
            }
            (Self::Assoc(m), Key::Index(i)) => {
                m.insert(i.to_string(), value);
            }
        }
    }

    pub fn remove(&mut self, key: &Key) {
        match (self, key) {
            (Self::Indexed(m), Key::Index(i)) => {
                m.remove(i);
            }
            (Self::Assoc(m), Key::Name(k)) => {
                m.remove(k);
            }
            (Self::Indexed(m), Key::Name(k)) => {
                if let Ok(i) = k.parse::<usize>() {
                    m.remove(&i);
                }
            }
            (Self::Assoc(m), Key::Index(i)) => {
                m.remove(&i.to_string());
            }
        }
    }

    /// The index one past the highest set one, where `a+=(x)` appends.
    #[must_use]
    pub fn next_index(&self) -> usize {
        match self {
            Self::Indexed(m) => m.last_key_value().map_or(0, |(i, _)| i + 1),
            Self::Assoc(_) => 0,
        }
    }

    /// Resolves a possibly negative index the way bash does: `-1` is the
    /// last element's index plus one, minus one. `None` if it falls before
    /// the start.
    #[must_use]
    pub fn resolve_index(&self, index: i64) -> Option<usize> {
        if index >= 0 {
            return usize::try_from(index).ok();
        }
        let end = i64::try_from(self.next_index()).ok()?;
        usize::try_from(end + index).ok()
    }

    /// The values, in order.
    pub fn values(&self) -> Box<dyn Iterator<Item = &str> + '_> {
        match self {
            Self::Indexed(m) => Box::new(m.values().map(String::as_str)),
            Self::Assoc(m) => Box::new(m.values().map(String::as_str)),
        }
    }

    /// The indices or keys, in order, as text.
    #[must_use]
    pub fn keys(&self) -> Vec<String> {
        match self {
            Self::Indexed(m) => m.keys().map(usize::to_string).collect(),
            Self::Assoc(m) => m.keys().cloned().collect(),
        }
    }

    /// The array as the body of a compound assignment, as `declare -p`
    /// prints it: `([0]="a" [1]="b c")`.
    #[must_use]
    pub fn to_source(&self) -> String {
        let mut out = String::from("(");
        match self {
            Self::Indexed(m) => {
                for (i, (k, v)) in m.iter().enumerate() {
                    if i > 0 {
                        out.push(' ');
                    }
                    let _ = write!(out, "[{k}]={}", double_quote(v));
                }
            }
            Self::Assoc(m) => {
                for (k, v) in m {
                    let key = if k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                        k.clone()
                    } else {
                        double_quote(k)
                    };
                    let _ = write!(out, "[{key}]={} ", double_quote(v));
                }
            }
        }
        out.push(')');
        out
    }
}

/// `s` quoted the way `declare -p` quotes values: in double quotes, with
/// the characters special there escaped, or when it holds control
/// characters, as a `$'...'` string.
#[must_use]
pub fn double_quote(s: &str) -> String {
    if s.chars().any(char::is_control) {
        return ansi_c_quote(s);
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if matches!(c, '"' | '\\' | '$' | '`') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// `s` quoted so that the shell reads it back as itself: in single quotes,
/// or when it holds control characters, as a `$'...'` string.
#[must_use]
pub fn quote_for_input(s: &str) -> String {
    if s.chars().any(char::is_control) {
        ansi_c_quote(s)
    } else {
        crate::expand::shell_quote_always(s)
    }
}

/// `s` as a `$'...'` string.
fn ansi_c_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 3);
    out.push_str("$'");
    for c in s.chars() {
        match c {
            '\x07' => out.push_str("\\a"),
            '\x08' => out.push_str("\\b"),
            '\x1b' => out.push_str("\\E"),
            '\x0c' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x0b' => out.push_str("\\v"),
            '\\' | '\'' => {
                out.push('\\');
                out.push(c);
            }
            c if c.is_control() => {
                let _ = write!(out, "\\{:03o}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}
