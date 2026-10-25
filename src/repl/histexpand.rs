//! History expansion at the prompt, as in bash (`set -H`): `!!`, `!n`,
//! `!-n`, `!text`, `!?text?`, with word designators (`:0`, `:n`, `^`, `$`,
//! `*`, `x-y`) and modifiers (`:h`, `:t`, `:r`, `:e`, `:p`, `:q`, `:x`,
//! `:s/old/new/`, `:gs/old/new/`, `:&`), and the `^old^new` shorthand.

/// A line after history expansion.
pub struct Expanded {
    pub line: String,
    /// A `:p` modifier: show the line, do not run it.
    pub print_only: bool,
}

/// Expands the history references in `line` against `history` (oldest
/// first). `Ok(None)` when there are none.
///
/// # Errors
///
/// Returns the message for a reference that matches no event, or a bad
/// word designator or modifier.
pub fn expand(line: &str, history: &[String]) -> Result<Option<Expanded>, String> {
    if !line.contains('!') && !line.starts_with('^') {
        return Ok(None);
    }
    let mut state = State {
        history,
        print_only: false,
        last_sub: None,
    };
    if let Some(rest) = line.strip_prefix('^') {
        let mut parts = rest.splitn(3, '^');
        let old = parts.next().unwrap_or_default();
        let new = parts.next().unwrap_or_default();
        let tail = parts.next().unwrap_or_default();
        let last = history.last().ok_or("!!: event not found")?;
        if !last.contains(old) || old.is_empty() {
            return Err("substitution failed".to_owned());
        }
        return Ok(Some(Expanded {
            line: format!("{}{tail}", last.replacen(old, new, 1)),
            print_only: false,
        }));
    }
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    let mut changed = false;
    let mut in_single = false;
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' {
            in_single = !in_single;
        } else if c == '\\' && !in_single && i + 1 < chars.len() {
            out.push(c);
            out.push(chars[i + 1]);
            i += 2;
            continue;
        } else if c == '!' && !in_single && starts_reference(chars.get(i + 1).copied()) {
            let (text, next) = state.reference(&chars, i + 1)?;
            out.push_str(&text);
            i = next;
            changed = true;
            continue;
        }
        out.push(c);
        i += 1;
    }
    Ok(changed.then_some(Expanded {
        line: out,
        print_only: state.print_only,
    }))
}

/// What may follow a `!` for it to begin a reference: not a blank, the end,
/// `=` or `(`.
fn starts_reference(next: Option<char>) -> bool {
    next.is_some_and(|c| !c.is_whitespace() && c != '=' && c != '(')
}

struct State<'h> {
    history: &'h [String],
    print_only: bool,
    last_sub: Option<(String, String)>,
}

impl State<'_> {
    /// The reference whose `!` came just before `chars[i]`: its expansion
    /// and the index just past it.
    fn reference(&mut self, chars: &[char], mut i: usize) -> Result<(String, usize), String> {
        let (event, mut words_default) = match chars[i] {
            '!' => {
                i += 1;
                (self.nth_back(1)?, None)
            }
            // `!$`, `!^`, `!*`: words of the last event.
            '$' | '^' | '*' => (self.nth_back(1)?, None),
            '#' => return Err("!#: not supported".to_owned()),
            '-' | '0'..='9' => {
                let start = i;
                i += 1;
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
                let spec: String = chars[start..i].iter().collect();
                let n: i64 = spec
                    .parse()
                    .map_err(|_| format!("!{spec}: event not found"))?;
                let event = if n < 0 {
                    self.nth_back(n.unsigned_abs())
                } else {
                    usize::try_from(n)
                        .ok()
                        .and_then(|n| n.checked_sub(1))
                        .and_then(|n| self.history.get(n).cloned())
                        .ok_or_else(|| format!("!{spec}: event not found"))
                }?;
                (event, None)
            }
            '?' => {
                let start = i + 1;
                let end = chars[start..]
                    .iter()
                    .position(|&c| c == '?')
                    .map_or(chars.len(), |p| start + p);
                let text: String = chars[start..end].iter().collect();
                i = (end + 1).min(chars.len());
                let event = self
                    .history
                    .iter()
                    .rev()
                    .find(|e| e.contains(&text))
                    .cloned()
                    .ok_or_else(|| format!("!?{text}: event not found"))?;
                (event, None)
            }
            _ => {
                let start = i;
                while i < chars.len()
                    && !chars[i].is_whitespace()
                    && !matches!(chars[i], ':' | ';' | '&' | '|' | '"')
                {
                    i += 1;
                }
                let text: String = chars[start..i].iter().collect();
                let event = self
                    .history
                    .iter()
                    .rev()
                    .find(|e| e.starts_with(&text))
                    .cloned()
                    .ok_or_else(|| format!("!{text}: event not found"))?;
                (event, None)
            }
        };
        let words = split_words(&event);
        // The shorthands `!$`, `!^`, `!*` are word designators without a `:`.
        if matches!(chars.get(i), Some('$' | '^' | '*'))
            && chars.get(i.wrapping_sub(1)) == Some(&'!')
        {
            words_default = Some(chars[i]);
        }
        let mut selected = match words_default {
            Some(d) => {
                i += 1;
                select(&words, &d.to_string())?
            }
            None if chars.get(i) == Some(&':')
                && chars.get(i + 1).is_some_and(|&c| is_designator_start(c)) =>
            {
                let start = i + 1;
                i = start;
                while i < chars.len()
                    && (chars[i].is_ascii_digit() || matches!(chars[i], '^' | '$' | '*' | '-'))
                {
                    i += 1;
                }
                let spec: String = chars[start..i].iter().collect();
                select(&words, &spec)?
            }
            None => event,
        };
        while chars.get(i) == Some(&':') {
            let (next, value) = self.modifier(chars, i + 1, &selected)?;
            i = next;
            selected = value;
        }
        Ok((selected, i))
    }

    /// The event `n` back from the end of history.
    fn nth_back(&self, n: u64) -> Result<String, String> {
        usize::try_from(n)
            .ok()
            .and_then(|n| self.history.len().checked_sub(n))
            .and_then(|i| self.history.get(i))
            .cloned()
            .ok_or_else(|| {
                format!(
                    "!{}: event not found",
                    if n == 1 {
                        "!".to_owned()
                    } else {
                        format!("-{n}")
                    }
                )
            })
    }

    /// Applies the modifier at `chars[i]` to `value`.
    fn modifier(
        &mut self,
        chars: &[char],
        mut i: usize,
        value: &str,
    ) -> Result<(usize, String), String> {
        let global = chars.get(i) == Some(&'g');
        if global {
            i += 1;
        }
        let Some(&m) = chars.get(i) else {
            return Err("bad modifier".to_owned());
        };
        i += 1;
        let path = std::path::Path::new(value);
        let result = match m {
            'h' => path
                .parent()
                .map_or_else(|| value.to_owned(), |p| p.to_string_lossy().into_owned()),
            't' => path
                .file_name()
                .map_or_else(|| value.to_owned(), |n| n.to_string_lossy().into_owned()),
            'r' => match value.rfind('.') {
                Some(dot) if !value[dot..].contains('/') => value[..dot].to_owned(),
                _ => value.to_owned(),
            },
            'e' => match value.rfind('.') {
                Some(dot) if !value[dot..].contains('/') => value[dot..].to_owned(),
                _ => String::new(),
            },
            'p' => {
                self.print_only = true;
                value.to_owned()
            }
            'q' => crate::expand::shell_quote_always(value),
            'x' => split_words(value)
                .iter()
                .map(|w| crate::expand::shell_quote_always(w))
                .collect::<Vec<_>>()
                .join(" "),
            's' | '&' => {
                let (old, new) = if m == 's' {
                    let delim = *chars.get(i).ok_or("bad substitution")?;
                    let mut fields = Vec::new();
                    let mut cur = String::new();
                    i += 1;
                    while i < chars.len() && fields.len() < 2 {
                        if chars[i] == delim {
                            fields.push(std::mem::take(&mut cur));
                        } else {
                            cur.push(chars[i]);
                        }
                        i += 1;
                    }
                    if fields.len() < 2 {
                        fields.push(cur);
                    }
                    let new = fields.pop().unwrap_or_default();
                    let old = fields.pop().unwrap_or_default();
                    let pair = (old, new);
                    self.last_sub = Some(pair.clone());
                    pair
                } else {
                    self.last_sub.clone().ok_or("no previous substitution")?
                };
                let new = new.replace('&', &old);
                if global {
                    value.replace(&old, &new)
                } else {
                    value.replacen(&old, &new, 1)
                }
            }
            other => return Err(format!("{other}: unrecognized history modifier")),
        };
        Ok((i, result))
    }
}

const fn is_designator_start(c: char) -> bool {
    c.is_ascii_digit() || matches!(c, '^' | '$' | '*' | '-')
}

/// The words of a history event, split on blanks outside quotes.
fn split_words(event: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut quote = None;
    for c in event.chars() {
        match (quote, c) {
            (None, '\'' | '"') => {
                quote = Some(c);
                cur.push(c);
            }
            (Some(q), _) if c == q => {
                quote = None;
                cur.push(c);
            }
            (None, c) if c.is_whitespace() => {
                if !cur.is_empty() {
                    words.push(std::mem::take(&mut cur));
                }
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words
}

/// The words `spec` designates: `n`, `^` (1), `$` (last), `*` (1 to last),
/// `x-y`, `x-` (to the one before last), `x*` (x to last).
fn select(words: &[String], spec: &str) -> Result<String, String> {
    let last = words.len().saturating_sub(1);
    let index = |s: &str| -> Result<usize, String> {
        match s {
            "^" => Ok(1),
            "$" => Ok(last),
            _ => s
                .parse()
                .map_err(|_| format!(":{spec}: bad word specifier")),
        }
    };
    let (from, to) = if spec == "*" {
        if words.len() <= 1 {
            return Ok(String::new());
        }
        (1, last)
    } else if let Some(x) = spec.strip_suffix('*') {
        (index(x)?, last)
    } else if let Some((x, y)) = spec.split_once('-') {
        let from = if x.is_empty() { 0 } else { index(x)? };
        let to = if y.is_empty() {
            last.saturating_sub(1)
        } else {
            index(y)?
        };
        (from, to)
    } else {
        let i = index(spec)?;
        (i, i)
    };
    if from > to || to > last {
        return Err(format!(":{spec}: bad word specifier"));
    }
    Ok(words[from..=to].join(" "))
}
