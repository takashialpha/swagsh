//! Assignment words: `name=value`, `name+=value`, `name[index]=value` and
//! `name=(items)`.

use anyhow::{Result, anyhow};

use crate::ast::{Assign, AssignValue, Word};
use crate::env::{Array, Key};
use crate::expand::{eval_arith_with, expand_tilde, is_name, shell_quote};

use super::Shell;

/// An assignment with everything in it expanded, ready to perform.
#[derive(Debug, Clone)]
pub struct ExpandedAssign {
    pub name: String,
    /// The subscript of `name[index]=value`, expanded but not yet evaluated.
    pub index: Option<String>,
    pub append: bool,
    pub value: ExpandedValue,
}

#[derive(Debug, Clone)]
pub enum ExpandedValue {
    Scalar(String),
    /// `name=(items)`: each item's subscript if it has one, whether it is
    /// `[index]+=value`, and its value.
    Array(Vec<(Option<String>, bool, String)>),
}

impl ExpandedAssign {
    /// `name=value` as written in a command, from `NAME=VALUE` or
    /// `NAME[INDEX]+=VALUE` text that did not come from an assignment word
    /// (a quoted argument of `declare`, say).
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let (head, value) = text.split_once('=')?;
        let (head, append) = head.strip_suffix('+').map_or((head, false), |h| (h, true));
        let (name, index) = match head.split_once('[') {
            Some((name, rest)) => (name, Some(rest.strip_suffix(']')?.to_owned())),
            None => (head, None),
        };
        is_name(name).then(|| Self {
            name: name.to_owned(),
            index,
            append,
            value: ExpandedValue::Scalar(value.to_owned()),
        })
    }

    /// How `set -x` shows it.
    #[must_use]
    pub fn display(&self) -> String {
        let mut out = self.name.clone();
        if let Some(index) = &self.index {
            out.push('[');
            out.push_str(index);
            out.push(']');
        }
        out.push_str(if self.append { "+=" } else { "=" });
        match &self.value {
            ExpandedValue::Scalar(value) => out.push_str(&quoted(value)),
            ExpandedValue::Array(items) => {
                out.push('(');
                for (i, (index, append, value)) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(' ');
                    }
                    if let Some(index) = index {
                        out.push('[');
                        out.push_str(index);
                        out.push_str(if *append { "]+=" } else { "]=" });
                    }
                    out.push_str(&quoted(value));
                }
                out.push(')');
            }
        }
        out
    }

    /// The `NAME=VALUE` a command's environment gets from it. An array
    /// becomes the text of its items in parentheses, as in bash.
    ///
    /// # Errors
    ///
    /// Returns an error for an element assignment, which cannot be one.
    pub fn environment_text(&self, current: Option<&str>) -> Result<String> {
        if self.index.is_some() {
            return Err(anyhow!("`{}[...]': not a valid identifier", self.name));
        }
        let value = match &self.value {
            ExpandedValue::Scalar(value) if self.append => {
                format!("{}{value}", current.unwrap_or_default())
            }
            ExpandedValue::Scalar(value) => value.clone(),
            ExpandedValue::Array(items) => {
                let values: Vec<&str> = items.iter().map(|(_, _, v)| v.as_str()).collect();
                format!("({})", values.join(" "))
            }
        };
        Ok(format!("{}={value}", self.name))
    }
}

/// `value` quoted for `set -x` when it would not read back as one word.
fn quoted(value: &str) -> String {
    if value.is_empty() {
        return "''".to_owned();
    }
    shell_quote(value)
}

impl Shell {
    /// Expands an assignment word. The value of a plain assignment is not
    /// field-split or pathname-expanded, and has its tildes expanded at the
    /// start and after each `:`; the items of `name=(items)` are expanded
    /// like a command's arguments.
    ///
    /// # Errors
    ///
    /// Returns an error if an expansion fails.
    pub fn expand_assign(&mut self, assign: &Assign) -> Result<ExpandedAssign> {
        let index = assign
            .index
            .as_ref()
            .map(|w| self.expand_word_to_string_inner(w, false))
            .transpose()?;
        let value = match &assign.value {
            AssignValue::Scalar(word) => ExpandedValue::Scalar(self.expand_assign_value(word)?),
            AssignValue::Array(items) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    if let Some(index) = &item.index {
                        let index = self.expand_word_to_string_inner(index, false)?;
                        let value = self.expand_assign_value(&item.value)?;
                        out.push((Some(index), item.append, value));
                    } else {
                        for value in self.expand_words(std::slice::from_ref(&item.value))? {
                            out.push((None, false, value));
                        }
                    }
                }
                ExpandedValue::Array(out)
            }
        };
        Ok(ExpandedAssign {
            name: assign.name.clone(),
            index,
            append: assign.append,
            value,
        })
    }

    /// The value of a plain assignment, with tildes expanded at its start
    /// and after each `:` in its leading literal text.
    fn expand_assign_value(&mut self, word: &Word) -> Result<String> {
        let (head, rest) = match word {
            Word::Literal(s) => (s.as_str(), &[][..]),
            Word::Compound(parts) => match parts.split_first() {
                Some((Word::Literal(s), rest)) => (s.as_str(), rest),
                _ => return self.expand_word_to_string_inner(word, false),
            },
            _ => return self.expand_word_to_string_inner(word, false),
        };
        let mut out = String::with_capacity(head.len());
        if head.contains('~') {
            for (i, segment) in head.split(':').enumerate() {
                if i > 0 {
                    out.push(':');
                }
                out.push_str(&expand_tilde(segment, &self.env));
            }
        } else {
            out.push_str(head);
        }
        for part in rest {
            out.push_str(&self.expand_word_to_string_inner(part, false)?);
        }
        Ok(out)
    }

    /// Expands and performs an assignment word, returning how `set -x`
    /// shows it when that is on.
    ///
    /// # Errors
    ///
    /// Returns an error if an expansion fails or the variable refuses the
    /// value.
    pub fn perform_assign(&mut self, assign: &Assign) -> Result<Option<String>> {
        // The common case, `name=value`, without building an
        // `ExpandedAssign`: this runs for every assignment a script makes.
        if let (None, false, AssignValue::Scalar(word), false) =
            (&assign.index, assign.append, &assign.value, self.xtrace)
        {
            let value = self.expand_assign_value(word)?;
            self.env.assign(&assign.name, value)?;
            return Ok(None);
        }
        let expanded = self.expand_assign(assign)?;
        self.apply_assign(&expanded)?;
        Ok(self.xtrace.then(|| expanded.display()))
    }

    /// Performs an expanded assignment.
    ///
    /// # Errors
    ///
    /// Returns an error if a subscript is not valid or the variable refuses
    /// the value.
    pub fn apply_assign(&mut self, assign: &ExpandedAssign) -> Result<()> {
        let name = assign.name.as_str();
        match (&assign.index, &assign.value) {
            (None, ExpandedValue::Scalar(value)) if assign.append => {
                self.env.append(name, value)?;
            }
            (None, ExpandedValue::Scalar(value)) => self.env.assign(name, value.as_str())?,
            (Some(index), ExpandedValue::Scalar(value)) => {
                let key = self.key_for(name, index)?;
                self.env
                    .assign_element(name, key, value.clone(), assign.append)?;
            }
            (None, ExpandedValue::Array(items)) => self.assign_array(name, items, assign.append)?,
            (Some(_), ExpandedValue::Array(_)) => {
                return Err(anyhow!("{name}: cannot assign a list to an array element"));
            }
        }
        Ok(())
    }

    /// `name=(items)`, or with `append`, `name+=(items)`.
    fn assign_array(
        &mut self,
        name: &str,
        items: &[(Option<String>, bool, String)],
        append: bool,
    ) -> Result<()> {
        let assoc = self.env.is_assoc(name);
        let mut array = match self.env.array(name) {
            Some(array) if append => array.clone(),
            _ => Array::new(assoc),
        };
        if append
            && self.env.array(name).is_none()
            && let Some(value) = self.env.get_ref(name)
        {
            array.set(array.first_key(), value.to_owned());
        }
        // bash 5.1 and later: the items of an associative array assignment
        // without subscripts are key, value, key, value...
        if assoc && items.iter().all(|(index, _, _)| index.is_none()) {
            for pair in items.chunks(2) {
                let value = pair.get(1).map_or_else(String::new, |(_, _, v)| v.clone());
                let value = self.env.element_value(name, value, None)?;
                array.set(Key::Name(pair[0].2.clone()), value);
            }
            self.env.set_array(name, array)?;
            return Ok(());
        }
        let mut next = array.next_index();
        for (index, item_append, value) in items {
            let key = match index {
                Some(index) if assoc => Key::Name(index.clone()),
                Some(index) => {
                    let i = eval_arith_with(index, &mut self.env)?;
                    let i = array
                        .resolve_index(i)
                        .ok_or_else(|| anyhow!("{name}[{index}]: bad array subscript"))?;
                    next = i + 1;
                    Key::Index(i)
                }
                None if assoc => {
                    return Err(anyhow!(
                        "{name}: {value}: must use subscript when assigning associative array"
                    ));
                }
                None => {
                    next += 1;
                    Key::Index(next - 1)
                }
            };
            let old = item_append.then(|| array.get(&key).unwrap_or_default().to_owned());
            let value = self
                .env
                .element_value(name, value.clone(), old.as_deref())?;
            array.set(key, value);
        }
        self.env.set_array(name, array)?;
        Ok(())
    }

    /// Assigns `value` to `target`, a variable name or an element
    /// (`name[index]`), as `read` and `printf -v` do.
    ///
    /// # Errors
    ///
    /// Returns an error if `target` is not a valid name or the variable
    /// refuses the value.
    pub fn assign_target(&mut self, target: &str, value: String) -> Result<()> {
        let mut assign = ExpandedAssign::parse(&format!("{target}="))
            .filter(|a| !a.append)
            .ok_or_else(|| anyhow!("`{target}': not a valid identifier"))?;
        assign.value = ExpandedValue::Scalar(value);
        self.apply_assign(&assign)
    }

    /// Whether `target`, a variable name or an element (`name[index]`), is
    /// set, as `test -v` asks.
    pub fn is_variable_set(&mut self, target: &str) -> bool {
        match target.split_once('[') {
            Some((name, rest)) if rest.ends_with(']') => {
                let index = &rest[..rest.len() - 1];
                if index == "@" || index == "*" {
                    return self.env.array(name).is_some_and(|a| !a.is_empty())
                        || self.env.get_ref(name).is_some();
                }
                self.key_for(name, index)
                    .is_ok_and(|key| self.env.element(name, &key).is_some())
            }
            _ => self.env.get_ref(target).is_some(),
        }
    }

    /// The element of `name` an expanded subscript names.
    ///
    /// # Errors
    ///
    /// Returns an error if the index is not a valid expression, or falls
    /// before the start of the array.
    pub fn key_for(&mut self, name: &str, index: &str) -> Result<Key> {
        let dynamic_assoc = matches!(name, "BASH_ALIASES" | "BASH_CMDS")
            && self.env.get_ref(name).is_none()
            && self.env.array(name).is_none();
        if dynamic_assoc || self.env.is_assoc(name) {
            return Ok(Key::Name(index.to_owned()));
        }
        if index == "@" || index == "*" {
            return Err(anyhow!("{name}[{index}]: bad array subscript"));
        }
        let i = match index.parse::<usize>() {
            Ok(i) => return Ok(Key::Index(i)),
            Err(_) => eval_arith_with(index, &mut self.env)?,
        };
        self.env
            .resolve_index(name, i)
            .map(Key::Index)
            .ok_or_else(|| anyhow!("{name}[{index}]: bad array subscript"))
    }
}
