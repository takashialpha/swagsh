//! Parameter expansion: what `$name` and `${...}` expand to, arrays and
//! the bash operators included.

use anyhow::{Result, anyhow};

use crate::env::{Array, Key, double_quote, quote_for_input};
use crate::errfmt::emit;
use crate::expand::{
    CaseChange, Param, ParamOp, ansi_c_unescape, change_case, eval_arith_with, find_unnested,
    is_name, parse_param_op, replace_matches, slice_range, strip_prefix, strip_suffix, substring,
};
use crate::parser::parse_word;

use super::Shell;
use super::expand::{has_param_op, is_plain_operand};

/// What a parameter expansion produced.
#[derive(Debug)]
pub(super) enum Expansion {
    One(String),
    /// `$@`, `${a[*]}` and the like. The `star` forms join into one field
    /// inside double quotes; the others give a field per item.
    List {
        items: Vec<String>,
        star: bool,
    },
}

impl Expansion {
    /// The expansion as one string: a list joined by the first character of
    /// `$IFS` for the `star` forms, by a space otherwise.
    pub(super) fn into_string(self, ifs: Option<&str>) -> String {
        match self {
            Self::One(s) => s,
            Self::List { items, star } => {
                let sep = if star {
                    ifs.map_or(" ", |ifs| {
                        ifs.get(..ifs.chars().next().map_or(0, char::len_utf8))
                            .unwrap_or("")
                    })
                } else {
                    " "
                };
                items.join(sep)
            }
        }
    }

    /// Null for the `:` forms of `${var:-word}` and friends: empty, or a
    /// list that joins to nothing.
    fn is_null(&self) -> bool {
        match self {
            Self::One(s) => s.is_empty(),
            Self::List { items, .. } => items.len() <= 1 && items.iter().all(String::is_empty),
        }
    }

    fn map(self, mut f: impl FnMut(&str) -> String) -> Self {
        match self {
            Self::One(s) => Self::One(f(&s)),
            Self::List { items, star } => Self::List {
                items: items.iter().map(|s| f(s)).collect(),
                star,
            },
        }
    }
}

/// What a `${var<op>word}` reference resolved to.
pub(super) enum Conditional<'w> {
    Value(Expansion),
    /// The operand word, still to be expanded.
    Word(&'w str),
}

/// How `param` was written, for messages: `a[1]`, `@`.
fn display(param: &Param<'_>) -> String {
    param.index.map_or_else(
        || param.name.to_owned(),
        |index| format!("{}[{index}]", param.name),
    )
}

impl Shell {
    /// Expands the body of a `$name`/`${...}` reference to one string.
    /// `in_quotes` says whether it sits inside double quotes, which changes
    /// how the operand of `${var:-word}` and friends is parsed.
    pub(super) fn expand_var_in(&mut self, text: &str, in_quotes: bool) -> Result<String> {
        if !has_param_op(text) {
            return self.resolve_param_checked(text);
        }
        Ok(match self.expand_param(text, in_quotes)? {
            Expansion::One(s) => s,
            list @ Expansion::List { .. } => list.into_string(self.env.get_ref("IFS")),
        })
    }

    /// Expands the body of a `${...}` reference, keeping a list a list.
    pub(super) fn expand_param(&mut self, text: &str, in_quotes: bool) -> Result<Expansion> {
        let op = parse_param_op(text).ok_or_else(|| anyhow!("${{{text}}}: bad substitution"))?;
        self.expand_param_op(op, in_quotes)
    }

    pub(super) fn expand_param_op(
        &mut self,
        op: ParamOp<'_>,
        in_quotes: bool,
    ) -> Result<Expansion> {
        Ok(match op {
            ParamOp::Plain(var) => self.value_checked(&var)?,
            ParamOp::Length(var) => {
                let n = match self.value_checked(&var)? {
                    Expansion::One(s) => s.chars().count(),
                    Expansion::List { items, .. } => items.len(),
                };
                Expansion::One(n.to_string())
            }
            ParamOp::Keys(var) => {
                let items = match self.env.array(var.name) {
                    Some(array) => array.keys(),
                    None if self.env.get_ref(var.name).is_some() => vec!["0".to_owned()],
                    None => Vec::new(),
                };
                Expansion::List {
                    items,
                    star: var.is_star(),
                }
            }
            ParamOp::Names { prefix, star } => {
                let env = &self.env;
                let items = env
                    .declared_names()
                    .into_iter()
                    .filter(|n| {
                        n.starts_with(prefix)
                            && (env.get_ref(n).is_some() || env.array(n).is_some())
                    })
                    .map(str::to_owned)
                    .collect();
                Expansion::List { items, star }
            }
            ParamOp::PrefixStrip { var, pat, greedy } => {
                let value = self.value_checked(&var)?;
                let pat = self.expand_param_pattern(pat, in_quotes)?;
                value.map(|s| strip_prefix(s, &pat, greedy))
            }
            ParamOp::SuffixStrip { var, pat, greedy } => {
                let value = self.value_checked(&var)?;
                let pat = self.expand_param_pattern(pat, in_quotes)?;
                value.map(|s| strip_suffix(s, &pat, greedy))
            }
            ParamOp::Conditional { var, op, word } => {
                match self.resolve_conditional(&var, op, word, in_quotes)? {
                    Conditional::Value(value) => value,
                    Conditional::Word(word) => {
                        Expansion::One(self.expand_param_word(word, in_quotes)?)
                    }
                }
            }
            ParamOp::Substring {
                var,
                offset,
                length,
            } => {
                let offset = self.arith_operand(offset)?;
                let length = length.map(|l| self.arith_operand(l)).transpose()?;
                self.substring(&var, offset, length)?
            }
            ParamOp::Replace {
                var,
                pat,
                rep,
                anchor,
            } => {
                let value = self.value_checked(&var)?;
                let pat = self.expand_param_pattern(pat, in_quotes)?;
                let pieces = match rep {
                    Some(rep) => self.replacement_pieces(rep, in_quotes)?,
                    None => vec![String::new()],
                };
                value.map(|s| replace_matches(s, &pat, anchor, |matched| pieces.join(matched)))
            }
            ParamOp::Transform { var, op } => self.transform(&var, op)?,
            ParamOp::Case { var, to, all, pat } => {
                let value = self.value_checked(&var)?;
                let pat = self.expand_param_pattern(pat, in_quotes)?;
                value.map(|s| change_case(s, to, all, &pat))
            }
        })
    }

    /// `${var@op}`; see [`ParamOp::Transform`].
    fn transform(&mut self, var: &Param<'_>, op: char) -> Result<Expansion> {
        let name = var.name;
        let whole_array = var.is_list() && is_name(name);
        match op {
            'a' => {
                let letters = crate::builtins::declare::attribute_letters(self, name);
                return Ok(Expansion::One(letters));
            }
            'A' if whole_array && self.env.array(name).is_some() => {
                let line = crate::builtins::declare::declaration_line(self, name);
                return Ok(Expansion::One(line.unwrap_or_default()));
            }
            'K' | 'k' if whole_array && self.env.array(name).is_some() => {
                let array = self
                    .env
                    .array(name)
                    .cloned()
                    .unwrap_or_else(|| Array::new(false));
                let pairs = array
                    .keys()
                    .into_iter()
                    .zip(array.values().map(str::to_owned));
                let items = if op == 'K' {
                    pairs
                        .map(|(k, v)| format!("{k} {}", double_quote(&v)))
                        .collect()
                } else {
                    pairs.flat_map(<[String; 2]>::from).collect()
                };
                return Ok(Expansion::List {
                    items,
                    star: var.is_star(),
                });
            }
            _ => {}
        }
        let Some(value) = self.lookup(var)? else {
            return Ok(Self::empty(var));
        };
        if op == 'A' {
            let value = value.into_string(self.env.get_ref("IFS"));
            let letters = crate::builtins::declare::attribute_letters(self, name);
            let assign = format!("{name}={}", quote_for_input(&value));
            return Ok(Expansion::One(if letters.is_empty() {
                assign
            } else {
                format!("declare -{letters} {assign}")
            }));
        }
        if op == 'P' {
            let mut items = Vec::new();
            let star = matches!(value, Expansion::List { star: true, .. });
            let list = matches!(value, Expansion::List { .. });
            for item in match value {
                Expansion::One(s) => vec![s],
                Expansion::List { items, .. } => items,
            } {
                items.push(crate::prompt::expand_prompt(self, &item));
            }
            return Ok(if list {
                Expansion::List { items, star }
            } else {
                Expansion::One(items.pop().unwrap_or_default())
            });
        }
        Ok(value.map(|s| match op {
            'Q' | 'K' => quote_for_input(s),
            'E' => ansi_c_unescape(s),
            'U' => s.to_uppercase(),
            'L' => s.to_lowercase(),
            'u' => change_case(s, CaseChange::Upper, false, ""),
            _ => s.to_owned(),
        }))
    }

    /// The replacement of `${var/pat/rep}`, split where it has an unquoted
    /// `&`, which stands for the matched text as in bash 5.2 and later.
    fn replacement_pieces(&mut self, raw: &str, in_quotes: bool) -> Result<Vec<String>> {
        let mut pieces = Vec::new();
        let mut rest = raw;
        while let Some(i) = find_unnested(rest, |c| c == '&') {
            pieces.push(self.expand_param_word(&rest[..i], in_quotes)?);
            rest = &rest[i + 1..];
        }
        pieces.push(self.expand_param_word(rest, in_quotes)?);
        Ok(pieces)
    }

    /// An arithmetic operand of `${var:offset:length}`.
    fn arith_operand(&mut self, raw: &str) -> Result<i64> {
        let expanded;
        let text = if raw.contains(['$', '`']) {
            expanded = self.expand_raw_text(raw)?;
            &expanded
        } else {
            raw
        };
        Ok(eval_arith_with(text, &mut self.env)?)
    }

    /// `${var:offset:length}`. On a string it counts characters; on an
    /// array, indices, so that gaps are skipped; on `$@`, positions with
    /// `$0` as position 0.
    fn substring(
        &mut self,
        var: &Param<'_>,
        offset: i64,
        length: Option<i64>,
    ) -> Result<Expansion> {
        if length.is_some_and(|l| l < 0) && var.is_list() {
            return Err(anyhow!("{}: substring expression < 0", display(var)));
        }
        let star = var.is_star();
        if matches!(var.name, "@" | "*") && !var.indirect {
            let mut all = Vec::with_capacity(self.env.positional_args().len() + 1);
            all.push(self.arg0.clone());
            all.extend(self.env.positional_args().iter().cloned());
            let items = slice_range(all.len(), offset, length)
                .map(|r| all[r].to_vec())
                .unwrap_or_default();
            return Ok(Expansion::List { items, star });
        }
        if var.is_list()
            && !var.indirect
            && let Some(array) = self.env.array(var.name)
            && !array.is_assoc()
        {
            let Some(start) = array.resolve_index(offset) else {
                return Ok(Expansion::List {
                    items: Vec::new(),
                    star,
                });
            };
            let take = length.map_or(usize::MAX, |l| usize::try_from(l).unwrap_or(0));
            let keys = array.keys();
            let items = keys
                .iter()
                .filter_map(|k| k.parse::<usize>().ok())
                .filter(|&i| i >= start)
                .take(take)
                .filter_map(|i| array.get(&Key::Index(i)).map(str::to_owned))
                .collect();
            return Ok(Expansion::List { items, star });
        }
        Ok(match self.value_checked(var)? {
            Expansion::One(s) => Expansion::One(substring(&s, offset, length)),
            Expansion::List { items, star } => Expansion::List {
                items: slice_range(items.len(), offset, length)
                    .map(|r| items[r].to_vec())
                    .unwrap_or_default(),
                star,
            },
        })
    }

    /// Decides what `${var<op>word}` expands to. The colon forms (`:-`, `:+`,
    /// `:?`, `:=`) treat an empty `var` like an unset one; the others only
    /// react to unset. Exempt from `nounset`, since every form handles the
    /// unset case itself.
    pub(super) fn resolve_conditional<'w>(
        &mut self,
        var: &Param<'_>,
        op: &str,
        word: &'w str,
        in_quotes: bool,
    ) -> Result<Conditional<'w>> {
        let value = self.lookup(var)?;
        let (missing, base) = op.strip_prefix(':').map_or_else(
            || (value.is_none(), op),
            |base| (value.as_ref().is_none_or(Expansion::is_null), base),
        );
        Ok(match (base, missing) {
            ("-", true) | ("+", false) => Conditional::Word(word),
            ("+", true) => Conditional::Value(Self::empty(var)),
            ("?", true) => {
                let msg = if word.is_empty() {
                    "parameter not set".to_owned()
                } else {
                    self.expand_param_word(word, in_quotes)?
                };
                let msg = format!("{}: {msg}", display(var));
                if self.interactive {
                    return Err(anyhow!(msg));
                }
                emit(msg);
                self.exit(1);
            }
            ("=", true) => {
                let expanded = self.expand_param_word(word, in_quotes)?;
                match var.index {
                    _ if !is_name(var.name) || var.indirect || var.is_list() => {
                        return Err(anyhow!("${}: cannot assign in this way", display(var)));
                    }
                    Some(raw) => {
                        let key = self.subscript_key(var.name, raw)?;
                        self.env
                            .assign_element(var.name, key, expanded.clone(), false)?;
                    }
                    None => self.env.assign(var.name, &expanded)?,
                }
                Conditional::Value(Expansion::One(expanded))
            }
            _ => Conditional::Value(value.unwrap_or_else(|| Self::empty(var))),
        })
    }

    /// Nothing, in the shape `var` expands to.
    fn empty(var: &Param<'_>) -> Expansion {
        if var.is_list() {
            Expansion::List {
                items: Vec::new(),
                star: var.is_star(),
            }
        } else {
            Expansion::One(String::new())
        }
    }

    /// Expands the operand of `${var:-word}` and friends as one string.
    pub(super) fn expand_param_word(&mut self, raw: &str, in_quotes: bool) -> Result<String> {
        if is_plain_operand(raw) {
            return Ok(raw.to_owned());
        }
        let word = parse_word(raw, in_quotes, Some(&self.env.aliases()))?;
        if in_quotes {
            self.expand_word_to_string_inner(&word, true)
        } else {
            self.expand_word_to_string(&word)
        }
    }

    /// Expands the pattern of `${var#pattern}` and friends. The pattern stays
    /// active even inside double quotes; only its own quoted parts are literal.
    fn expand_param_pattern(&mut self, raw: &str, in_quotes: bool) -> Result<String> {
        if is_plain_operand(raw) {
            return Ok(raw.to_owned());
        }
        let word = parse_word(raw, in_quotes, Some(&self.env.aliases()))?;
        self.expand_pattern(&word)
    }

    /// The element of array `name` that subscript `raw` (still unexpanded)
    /// names: a key for an associative array, otherwise an arithmetic
    /// index, negative ones counting back from the end.
    ///
    /// # Errors
    ///
    /// Returns an error if expanding the subscript fails, or the index is
    /// before the start of the array.
    pub(crate) fn subscript_key(&mut self, name: &str, raw: &str) -> Result<Key> {
        let text = if is_plain_operand(raw) {
            raw.to_owned()
        } else {
            let word = parse_word(raw, false, Some(&self.env.aliases()))?;
            self.expand_word_to_string_inner(&word, false)?
        };
        self.key_for(name, &text)
    }

    /// The value of `param`, or `None` if it is unset.
    fn lookup(&mut self, param: &Param<'_>) -> Result<Option<Expansion>> {
        if param.indirect {
            // `${!ref}` of a reference is the name it refers to.
            if let Some(target) = self.env.nameref_target(param.name) {
                return Ok(Some(Expansion::One(target)));
            }
            let direct = Param {
                indirect: false,
                ..*param
            };
            let Some(target) = self.lookup(&direct)? else {
                return Ok(None);
            };
            let target = target.into_string(self.env.get_ref("IFS"));
            let parsed =
                parse_param_op(&target).filter(|op| matches!(op, ParamOp::Plain(p) if !p.indirect));
            let Some(ParamOp::Plain(inner)) = parsed else {
                return Err(anyhow!("{target}: invalid indirect expansion"));
            };
            return self.lookup(&inner);
        }
        Ok(match (param.name, param.index) {
            ("@" | "*", _) => Some(Expansion::List {
                items: self.env.positional_args().to_vec(),
                star: param.name == "*",
            }),
            (name, _) if !is_name(name) => self
                .is_param_set(name)
                .then(|| Expansion::One(self.resolve_param(name))),
            (name, None) => self
                .env
                .get_ref(name)
                .map(str::to_owned)
                .or_else(|| self.dynamic_var(name))
                .map(Expansion::One),
            (name, Some(index @ ("@" | "*"))) => {
                let items = match self.env.array(name) {
                    // An array with no elements counts as unset, as in bash.
                    Some(array) if array.is_empty() => None,
                    Some(array) => Some(array.values().map(str::to_owned).collect()),
                    None => self.env.get_ref(name).map_or_else(
                        || {
                            self.dynamic_array(name)
                                .map(|a| a.values().map(str::to_owned).collect())
                        },
                        |v| Some(vec![v.to_owned()]),
                    ),
                };
                items.map(|items| Expansion::List {
                    items,
                    star: index == "*",
                })
            }
            (name, Some(raw)) => {
                let key = self.subscript_key(name, raw)?;
                match self.env.element(name, &key) {
                    Some(v) => Some(Expansion::One(v.to_owned())),
                    None if self.env.get_ref(name).is_none() && self.env.array(name).is_none() => {
                        self.dynamic_array(name)
                            .and_then(|a| a.get(&key).map(|v| Expansion::One(v.to_owned())))
                    }
                    None => None,
                }
            }
        })
    }

    /// [`Shell::lookup`] for a direct reference, which honors `nounset`.
    fn value_checked(&mut self, param: &Param<'_>) -> Result<Expansion> {
        if let Some(value) = self.lookup(param)? {
            return Ok(value);
        }
        // bash exempts every list form, `${a[@]}` of an unset `a` included.
        if !param.is_list() {
            self.unbound(&display(param))?;
        }
        Ok(Self::empty(param))
    }

    /// Resolves a bare parameter name to its value: the special parameters
    /// (`$?`, `$$`, `$0`, `$1`.., `$@`/`$*`, `$#`) plus a plain variable
    /// lookup for everything else. Never fails: `""` for anything unset,
    /// whatever `nounset` says.
    pub(super) fn resolve_param(&self, name: &str) -> String {
        match name {
            "?" => self.last_status.0.to_string(),
            "!" => self
                .last_bg_pid
                .map(|p| p.as_raw_nonzero().to_string())
                .unwrap_or_default(),
            "$" => self.shell_pid.to_string(),
            "-" => self.option_flags(),
            "0" => self.arg0.clone(),
            n if n.chars().all(|c| c.is_ascii_digit()) => {
                let idx: usize = n.parse().unwrap_or(0);
                self.env
                    .positional_args()
                    .get(idx.saturating_sub(1))
                    .cloned()
                    .unwrap_or_default()
            }
            "@" | "*" => self.env.positional_args().join(" "),
            "#" => self.env.positional_args().len().to_string(),
            name => self
                .env
                .get(name)
                .or_else(|| self.dynamic_var(name))
                .unwrap_or_default(),
        }
    }

    /// The variables whose values the shell works out when they are read:
    /// `RANDOM`, `SECONDS`, `LINENO`, the `BASH_*` ones, and the first
    /// elements of the arrays among them.
    fn dynamic_var(&self, name: &str) -> Option<String> {
        match name {
            "LINENO" => Some(self.lineno.to_string()),
            "PIPESTATUS" | "FUNCNAME" | "BASH_SOURCE" | "BASH_LINENO" => {
                self.dynamic_array(name)?.values().next().map(str::to_owned)
            }
            "BASHPID" => Some(rustix::process::getpid().as_raw_nonzero().to_string()),
            "BASH_SUBSHELL" => Some(self.subshell.to_string()),
            "BASH_COMMAND" => Some(self.bash_command.clone()),
            "BASH_ARGV0" => Some(self.arg0.clone()),
            "BASH_EXECUTION_STRING" => self.command_string.clone(),
            "BASHOPTS" => Some(crate::builtins::shopt_on(self).join(":")),
            _ => self.env.dynamic(name),
        }
    }

    /// `PIPESTATUS`, `FUNCNAME`, `BASH_SOURCE`, `BASH_LINENO`,
    /// `BASH_ALIASES` and `BASH_CMDS`, when no variable of that name hides
    /// them. `FUNCNAME` and `BASH_LINENO` are unset outside functions.
    pub(super) fn dynamic_array(&self, name: &str) -> Option<Array> {
        let in_function = !self.frames.is_empty();
        let main = (in_function && self.script).then(|| "main".to_owned());
        let items: Vec<String> = match name {
            "PIPESTATUS" => self.pipestatus.iter().map(i32::to_string).collect(),
            "FUNCNAME" if in_function => self
                .frames
                .iter()
                .rev()
                .map(|f| f.name.clone())
                .chain(main)
                .collect(),
            "BASH_SOURCE" if in_function || !self.sources.is_empty() => {
                let files = self.frames.iter().rev().map(|f| f.file.to_string());
                let outer = self.sources.iter().rev().map(ToString::to_string);
                files.chain(outer.take(1)).collect()
            }
            "BASH_LINENO" if in_function => self
                .frames
                .iter()
                .rev()
                .map(|f| f.line.to_string())
                .chain(main.map(|_| "0".to_owned()))
                .collect(),
            "BASH_ALIASES" | "BASH_CMDS" => {
                let mut array = Array::new(true);
                let pairs: Vec<(String, String)> = if name == "BASH_ALIASES" {
                    self.env
                        .all_aliases()
                        .map(|(k, v)| (k.to_owned(), v.to_owned()))
                        .collect()
                } else {
                    self.env.hashed_commands()
                };
                for (k, v) in pairs {
                    array.set(Key::Name(k), v);
                }
                return Some(array);
            }
            _ => return None,
        };
        let mut array = Array::new(false);
        for (i, item) in items.into_iter().enumerate() {
            array.set(Key::Index(i), item);
        }
        Some(array)
    }

    /// Whether `name` counts as "set" for `nounset` purposes: the special
    /// parameters are always set (even `$!` before any background job has
    /// run), a positional parameter is set only within `$#`'s current
    /// range, and anything else is set if the variable has a value.
    fn is_param_set(&self, name: &str) -> bool {
        match name {
            "?" | "!" | "$" | "0" | "@" | "*" | "#" | "-" => true,
            n if n.chars().all(|c| c.is_ascii_digit()) => {
                let idx: usize = n.parse().unwrap_or(0);
                idx >= 1 && idx <= self.env.positional_args().len()
            }
            name => self.env.get_ref(name).is_some() || self.dynamic_var(name).is_some(),
        }
    }

    /// Like `resolve_param`, but honors `-u`/`nounset`: fails instead of
    /// silently returning `""` if `name` is unset. Every direct reference
    /// goes through here; the operators that handle the unset case
    /// themselves (`:-` and friends) use `resolve_param`.
    pub(super) fn resolve_param_checked(&mut self, name: &str) -> Result<String> {
        if self.nounset && !self.is_param_set(name) {
            self.unbound(name)?;
        }
        Ok(self.resolve_param(name))
    }

    /// Reports a reference to unset `name` under `nounset`. A
    /// non-interactive shell exits, as for a syntax error: with 127, or 1
    /// if `errexit` is also on. An interactive one fails the command.
    fn unbound(&mut self, name: &str) -> Result<()> {
        if !self.nounset {
            return Ok(());
        }
        if self.interactive {
            anyhow::bail!("{name}: unbound variable");
        }
        emit(format!("{name}: unbound variable"));
        self.exit(if self.errexit { 1 } else { 127 });
    }
}
