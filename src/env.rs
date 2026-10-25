use std::rc::Rc;

// `rustc-hash`, not `std::collections::HashMap`'s default `SipHash`:
// `SipHash`'s whole point is DoS-resistance against an attacker who
// controls the keys going into a hash table this process doesn't fully
// trust, e.g. a network service hashing request headers. A shell's own
// variable table has no such adversary: whoever can set enough variables
// to try to degrade this table already has arbitrary code execution in
// the shell, so there's nothing to defend against and no reason to pay
// for it. Benchmarked head-to-head against `ahash` and `foldhash` (both
// also DoS-resistant-by-default, also candidates for this exact reason);
// all three perform identically for the short, ordinary keys these tables
// hold (variable/function/alias names), so the deciding factor is
// implementation weight: `rustc-hash` is what it sounds like, rustc's own
// hasher for compiler-internal symbol tables (the same shape of problem
// as this one), a couple hundred lines, no `unsafe`, no runtime feature
// detection, no per-process random seed to generate. `Env::get`/`set` sit
// on every variable read and write, so this is a genuinely hot path.
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use crate::ast::Command;

mod array;

pub use array::{Array, Key, double_quote, quote_for_input};

/// Alias names and their values.
pub type AliasMap = HashMap<String, String>;

/// Sets a process environment variable. Single-threaded shell: no concurrent
/// env mutation, so this is always safe to call.
fn set_process_var(name: &str, value: &str) {
    // SAFETY: single-threaded shell: no concurrent env mutation.
    unsafe { std::env::set_var(name, value) }
}

/// Removes a process environment variable. Single-threaded shell: no
/// concurrent env mutation, so this is always safe to call.
fn remove_process_var(name: &str) {
    // SAFETY: single-threaded shell: no concurrent env mutation.
    unsafe { std::env::remove_var(name) }
}

#[derive(Debug, Clone)]
pub struct Env {
    vars: HashMap<String, String>,
    exported: HashSet<String>,
    functions: HashMap<String, Rc<Command>>,
    function_sources: HashMap<String, Rc<str>>,
    /// Shared with the parser, which takes a snapshot for each command it
    /// reads; definitions copy on write.
    aliases: Rc<AliasMap>,
    positional: Vec<String>,
    /// `set -a`: every variable assigned is exported too.
    pub allexport: bool,
    /// `shopt expand_aliases`: the parser expands aliases.
    pub expand_aliases: bool,
    readonly: HashSet<String>,
    /// One frame per running function, holding what each `local` variable
    /// was before it, to put back when the function returns.
    local_frames: Vec<Vec<SavedVar>>,
    /// `hash`: where each external command was found on `$PATH`. Cleared
    /// whenever `PATH` changes.
    command_hash: HashMap<String, String>,
    /// Array variables. A name is in this table or in `vars`, never both,
    /// so the plain variables keep their own fast table.
    arrays: HashMap<String, Array>,
    /// Variables given attributes by `declare` and friends. Being here at
    /// all means declared, even with no value yet (`declare -a a`).
    attrs: HashMap<String, Attrs>,
    /// `$RANDOM`'s generator state; assigning `RANDOM` seeds it.
    random: std::cell::Cell<u32>,
    /// When `$SECONDS` was last assigned, and the value it was given.
    seconds: (std::time::Instant, i64),
}

/// The attributes `declare` gives a variable, beyond `-r` and `-x`, which
/// have their own sets.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Attrs(u8);

impl Attrs {
    /// `-a`: an indexed array.
    pub const INDEXED: Self = Self(1);
    /// `-A`: an associative array.
    pub const ASSOC: Self = Self(1 << 1);
    /// `-i`: assignments are evaluated as arithmetic.
    pub const INTEGER: Self = Self(1 << 2);
    /// `-l`: assignments are lowercased.
    pub const LOWER: Self = Self(1 << 3);
    /// `-u`: assignments are uppercased.
    pub const UPPER: Self = Self(1 << 4);
    /// `-n`: a reference, whose value is the name of the variable that
    /// reads and assignments go to.
    pub const NAMEREF: Self = Self(1 << 5);

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0 && other.0 != 0
    }

    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    #[must_use]
    pub const fn without(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }
}

/// A variable's value: a plain string or an array.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Scalar(String),
    Array(Array),
}

/// A variable's state before `local` shadowed it.
#[derive(Debug, Clone)]
struct SavedVar {
    name: String,
    value: Option<Value>,
    exported: bool,
    attrs: Option<Attrs>,
}

/// An assignment the variable refused.
#[derive(Debug)]
pub enum AssignError {
    Readonly(String),
    /// The value of an integer variable is not a valid expression.
    Arith(String),
    /// A plain variable cannot become an associative array, nor an
    /// associative array an indexed one or the other way round.
    Conversion(String),
}

impl std::fmt::Display for AssignError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Readonly(name) => write!(f, "{name}: readonly variable"),
            Self::Arith(msg) => f.write_str(msg),
            Self::Conversion(name) => write!(f, "{name}: cannot convert between array kinds"),
        }
    }
}

impl std::error::Error for AssignError {}

/// An assignment to, or `unset` of, a readonly variable.
#[derive(Debug)]
pub struct ReadonlyError(pub String);

impl std::fmt::Display for ReadonlyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: readonly variable", self.0)
    }
}

impl std::error::Error for ReadonlyError {}

impl Env {
    #[must_use]
    pub fn from_process() -> Self {
        let raw: Vec<(String, String)> = std::env::vars().collect();
        let cap = raw.len() + 8;
        let mut vars = HashMap::with_capacity_and_hasher(cap, rustc_hash::FxBuildHasher);
        let mut exported = HashSet::with_capacity_and_hasher(cap, rustc_hash::FxBuildHasher);
        for (k, v) in raw {
            exported.insert(k.clone());
            vars.insert(k, v);
        }
        vars.entry("PATH".into())
            .or_insert_with(|| "/usr/local/bin:/usr/bin:/bin".into());
        vars.entry("HOME".into())
            .or_insert_with(|| std::env::var("HOME").unwrap_or_else(|_| "/".into()));
        vars.entry("IFS".into()).or_insert_with(|| " \t\n".into());
        vars.insert(
            "PPID".into(),
            rustix::process::getppid()
                .map_or(0, |p| p.as_raw_nonzero().get())
                .to_string(),
        );
        vars.insert("UID".into(), rustix::process::getuid().as_raw().to_string());
        vars.insert(
            "EUID".into(),
            rustix::process::geteuid().as_raw().to_string(),
        );
        // Preserve the inherited $PWD (logical path through symlinks set by the
        // parent shell). Only fall back to the kernel cwd if $PWD is absent.
        if vars.contains_key("PWD") {
            exported.insert("PWD".into());
        } else if let Ok(cwd) = std::env::current_dir() {
            let s = cwd.to_string_lossy().into_owned();
            exported.insert("PWD".into());
            vars.insert("PWD".into(), s);
        }
        let env = Self {
            vars,
            exported,
            functions: HashMap::default(),
            function_sources: HashMap::default(),
            aliases: Rc::default(),
            positional: Vec::new(),
            allexport: false,
            expand_aliases: true,
            readonly: HashSet::default(),
            local_frames: Vec::new(),
            command_hash: HashMap::default(),
            arrays: HashMap::default(),
            attrs: HashMap::default(),
            random: std::cell::Cell::new(std::process::id() ^ 0x9e37_79b9),
            seconds: (std::time::Instant::now(), 0),
        };
        env.update_collation();
        env
    }

    // ------------------------------------------------------------------
    // Variables
    // ------------------------------------------------------------------

    #[inline]
    #[must_use]
    pub fn get(&self, name: &str) -> Option<String> {
        self.get_ref(name).map(str::to_owned)
    }
    /// [`Env::get`] without the copy. For an array, its first element (see
    /// [`Array::first_key`]).
    #[inline]
    #[must_use]
    pub fn get_ref(&self, name: &str) -> Option<&str> {
        if let Some(target) = self.nameref_target(name) {
            return match split_subscript(&target) {
                Some((base, index)) => self.element(base, &self.literal_key(base, index)),
                None => self.get_ref(&target),
            };
        }
        match self.vars.get(name) {
            Some(v) => Some(v),
            None if self.arrays.is_empty() => None,
            None => self.arrays.get(name).and_then(|a| a.get(&a.first_key())),
        }
    }
    #[inline]
    #[must_use]
    pub fn get_or_empty(&self, name: &str) -> String {
        self.get(name).unwrap_or_default()
    }

    /// Sets `name` with no check of its attributes. On an array, sets its
    /// first element.
    pub fn set(&mut self, name: &str, value: impl Into<String>) {
        let value = value.into();
        if let Some(target) = self.nameref_target(name) {
            match split_subscript(&target) {
                Some((base, index)) => {
                    let key = self.literal_key(base, index);
                    if let Ok(array) = self.array_entry(base) {
                        array.set(key, value);
                    }
                }
                None => self.set(&target, value),
            }
            return;
        }
        if !self.arrays.is_empty()
            && let Some(array) = self.arrays.get_mut(name)
        {
            array.set(array.first_key(), value);
            return;
        }
        if self.allexport && !self.exported.contains(name) {
            self.exported.insert(name.to_owned());
        }
        if self.exported.contains(name) {
            set_process_var(name, &value);
        }
        if let Some(slot) = self.vars.get_mut(name) {
            *slot = value;
        } else {
            self.vars.insert(name.to_owned(), value);
        }
        self.var_changed(name);
    }

    /// Keeps what depends on a variable in step with it: the command hash
    /// table on `PATH`, the glob collation on the locale variables.
    fn var_changed(&mut self, name: &str) {
        match name {
            "PATH" => self.command_hash.clear(),
            "LC_ALL" | "LC_COLLATE" | "LANG" => self.update_collation(),
            // Assigning these sets what they count from; they stay dynamic.
            "RANDOM" | "SECONDS" => {
                if let Some(value) = self.vars.remove(name) {
                    let n = value.trim().parse::<i64>().unwrap_or(0);
                    if name == "RANDOM" {
                        self.random
                            .set(u32::try_from(n & 0xffff_ffff).unwrap_or(0) ^ 0x9e37_79b9);
                    } else {
                        self.seconds = (std::time::Instant::now(), n);
                    }
                }
            }
            _ => {}
        }
    }

    /// `$RANDOM` (a new number from 0 to 32767 on every read) or `$SECONDS`
    /// (seconds since the shell started, or since it was assigned).
    #[must_use]
    pub fn dynamic(&self, name: &str) -> Option<String> {
        match name {
            "RANDOM" => {
                // xorshift32: plenty for a shell's random numbers.
                let mut x = self.random.get().max(1);
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                self.random.set(x);
                Some((x >> 8 & 0x7fff).to_string())
            }
            "SECONDS" => {
                let elapsed = i64::try_from(self.seconds.0.elapsed().as_secs()).unwrap_or(0);
                Some((self.seconds.1 + elapsed).to_string())
            }
            _ => None,
        }
    }

    fn update_collation(&self) {
        let locale = ["LC_ALL", "LC_COLLATE", "LANG"]
            .iter()
            .find_map(|v| self.vars.get(*v).filter(|s| !s.is_empty()));
        crate::collate::select(locale.map_or("C", String::as_str));
    }

    /// [`Env::set`] for an assignment the user wrote, which a readonly
    /// variable refuses and the variable's attributes transform.
    ///
    /// # Errors
    ///
    /// Returns an [`AssignError`] if `name` is readonly, or is an integer
    /// variable and `value` is not a valid expression.
    pub fn assign(&mut self, name: &str, value: impl Into<String>) -> Result<(), AssignError> {
        if let Some(target) = self.nameref_target(name) {
            return match split_subscript(&target) {
                Some((base, index)) => {
                    let key = self.literal_key(base, index);
                    self.assign_element(base, key, value.into(), false)
                }
                None => self.assign(&target, value),
            };
        }
        if self.readonly.contains(name) {
            return Err(AssignError::Readonly(name.to_owned()));
        }
        let value = self.transform(name, value.into(), None)?;
        self.set(name, value);
        Ok(())
    }

    /// `name+=value`: appends to the current value, or for an integer
    /// variable, adds to it.
    ///
    /// # Errors
    ///
    /// As for [`Env::assign`].
    pub fn append(&mut self, name: &str, value: &str) -> Result<(), AssignError> {
        if let Some(target) = self.nameref_target(name) {
            return match split_subscript(&target) {
                Some((base, index)) => {
                    let key = self.literal_key(base, index);
                    self.assign_element(base, key, value.to_owned(), true)
                }
                None => self.append(&target, value),
            };
        }
        if self.readonly.contains(name) {
            return Err(AssignError::Readonly(name.to_owned()));
        }
        let old = self.get_ref(name).unwrap_or_default().to_owned();
        let value = self.transform(name, value.to_owned(), Some(&old))?;
        self.set(name, value);
        Ok(())
    }

    /// Applies `name`'s attributes to a value being assigned to it, or with
    /// `old`, appended to `old`.
    fn transform(
        &mut self,
        name: &str,
        value: String,
        old: Option<&str>,
    ) -> Result<String, AssignError> {
        let join = |value: String| match old {
            Some(old) => format!("{old}{value}"),
            None => value,
        };
        if self.attrs.is_empty() {
            return Ok(join(value));
        }
        let attrs = match self.attrs.get(name) {
            Some(&attrs)
                if [Attrs::INTEGER, Attrs::LOWER, Attrs::UPPER]
                    .iter()
                    .any(|&a| attrs.contains(a)) =>
            {
                attrs
            }
            _ => return Ok(join(value)),
        };
        if attrs.contains(Attrs::INTEGER) {
            let eval = |env: &mut Self, expr: &str| {
                crate::expand::eval_arith_with(expr, env)
                    .map_err(|e| AssignError::Arith(e.to_string()))
            };
            let mut n = eval(self, &value)?;
            if let Some(old) = old {
                n = n.wrapping_add(eval(self, old)?);
            }
            return Ok(n.to_string());
        }
        let value = join(value);
        Ok(if attrs.contains(Attrs::LOWER) {
            value.to_lowercase()
        } else {
            value.to_uppercase()
        })
    }

    // ------------------------------------------------------------------
    // Arrays and attributes
    // ------------------------------------------------------------------

    #[must_use]
    pub fn array(&self, name: &str) -> Option<&Array> {
        if let Some(target) = self.nameref_target(name) {
            return self.array(base_name(&target));
        }
        self.arrays.get(name)
    }

    /// Whether `name` is, or is declared as, an associative array, whose
    /// subscripts are keys rather than arithmetic.
    #[must_use]
    pub fn is_assoc(&self, name: &str) -> bool {
        if let Some(target) = self.nameref_target(name) {
            return self.is_assoc(base_name(&target));
        }
        self.arrays.get(name).map_or_else(
            || {
                self.attrs
                    .get(name)
                    .is_some_and(|a| a.contains(Attrs::ASSOC))
            },
            Array::is_assoc,
        )
    }

    /// Resolves a possibly negative index into `name` (see
    /// [`Array::resolve_index`]); a plain variable counts as an array of
    /// one element.
    #[must_use]
    pub fn resolve_index(&self, name: &str, index: i64) -> Option<usize> {
        if let Some(target) = self.nameref_target(name) {
            return self.resolve_index(base_name(&target), index);
        }
        match self.arrays.get(name) {
            Some(array) => array.resolve_index(index),
            None if index >= 0 => usize::try_from(index).ok(),
            None => {
                let len = i64::from(self.vars.contains_key(name));
                usize::try_from(len + index).ok()
            }
        }
    }

    /// One element of `name`. A plain variable is its own element 0.
    #[must_use]
    pub fn element(&self, name: &str, key: &Key) -> Option<&str> {
        if let Some(target) = self.nameref_target(name) {
            return self.element(base_name(&target), key);
        }
        match self.arrays.get(name) {
            Some(array) => array.get(key),
            None if matches!(key, Key::Index(0)) || matches!(key, Key::Name(k) if k == "0") => {
                self.vars.get(name).map(String::as_str)
            }
            None => None,
        }
    }

    /// `name[key]=value`, or with `append`, `name[key]+=value`. A plain
    /// variable becomes an array holding its value at index 0.
    ///
    /// # Errors
    ///
    /// As for [`Env::assign`].
    pub fn assign_element(
        &mut self,
        name: &str,
        key: Key,
        value: String,
        append: bool,
    ) -> Result<(), AssignError> {
        if let Some(target) = self.nameref_target(name) {
            return self.assign_element(base_name(&target), key, value, append);
        }
        if self.readonly.contains(name) {
            return Err(AssignError::Readonly(name.to_owned()));
        }
        let old = append.then(|| self.element(name, &key).unwrap_or_default().to_owned());
        let value = self.transform(name, value, old.as_deref())?;
        self.array_entry(name)?.set(key, value);
        Ok(())
    }

    /// `name`'s array, made into one if it is a plain variable or unset.
    fn array_entry(&mut self, name: &str) -> Result<&mut Array, AssignError> {
        if !self.arrays.contains_key(name) {
            let assoc = self
                .attrs
                .get(name)
                .is_some_and(|a| a.contains(Attrs::ASSOC));
            let mut array = Array::new(assoc);
            if let Some(scalar) = self.vars.remove(name) {
                if assoc {
                    self.vars.insert(name.to_owned(), scalar);
                    return Err(AssignError::Conversion(name.to_owned()));
                }
                array.set(Key::Index(0), scalar);
                remove_process_var(name);
            }
            self.arrays.insert(name.to_owned(), array);
        }
        self.arrays
            .get_mut(name)
            .ok_or_else(|| AssignError::Conversion(name.to_owned()))
    }

    /// Replaces `name`'s value with `array`, whose values have already been
    /// through the variable's attributes.
    ///
    /// # Errors
    ///
    /// Returns an [`AssignError`] if `name` is readonly, or `array`'s kind
    /// is not the one `name` was declared with.
    pub fn set_array(&mut self, name: &str, array: Array) -> Result<(), AssignError> {
        if let Some(target) = self.nameref_target(name) {
            return self.set_array(base_name(&target), array);
        }
        if self.readonly.contains(name) {
            return Err(AssignError::Readonly(name.to_owned()));
        }
        if self.vars.remove(name).is_some() {
            remove_process_var(name);
        }
        self.arrays.insert(name.to_owned(), array);
        Ok(())
    }

    /// A value for an element of `name`, through its attributes, for
    /// building an array to hand [`Env::set_array`].
    ///
    /// # Errors
    ///
    /// As for [`Env::assign`].
    pub fn element_value(
        &mut self,
        name: &str,
        value: String,
        old: Option<&str>,
    ) -> Result<String, AssignError> {
        if let Some(target) = self.nameref_target(name) {
            return self.element_value(base_name(&target), value, old);
        }
        self.transform(name, value, old)
    }

    /// `unset 'name[key]'`.
    ///
    /// # Errors
    ///
    /// Returns an [`AssignError`] if `name` is readonly.
    pub fn unset_element(&mut self, name: &str, key: &Key) -> Result<(), AssignError> {
        if let Some(target) = self.nameref_target(name) {
            return self.unset_element(base_name(&target), key);
        }
        if self.readonly.contains(name) {
            return Err(AssignError::Readonly(name.to_owned()));
        }
        if let Some(array) = self.arrays.get_mut(name) {
            array.remove(key);
        } else if self.element(name, key).is_some() {
            self.unset_var(name);
        }
        Ok(())
    }

    /// Where reference `name` (`declare -n`) points, after following every
    /// reference in turn, or `None` when `name` is not one.
    #[inline]
    #[must_use]
    pub fn nameref_target(&self, name: &str) -> Option<String> {
        // Checked here, inlined into every variable access, rather than
        // after a call: almost no script declares anything.
        if self.attrs.is_empty() {
            return None;
        }
        self.follow_nameref(name)
    }

    #[inline(never)]
    fn follow_nameref(&self, name: &str) -> Option<String> {
        let mut current = None::<String>;
        // A loop of references ends, as in bash, after a bounded number.
        for _ in 0..16 {
            let at = current.as_deref().unwrap_or(name);
            let is_ref = self
                .attrs
                .get(at)
                .is_some_and(|a| a.contains(Attrs::NAMEREF));
            match self.vars.get(at).filter(|t| is_ref && !t.is_empty()) {
                Some(target) => current = Some(target.clone()),
                None => break,
            }
        }
        current
    }

    /// Makes `name` a reference to `target`.
    pub fn set_nameref(&mut self, name: &str, target: &str) {
        self.vars.insert(name.to_owned(), target.to_owned());
    }

    /// The key a subscript names without arithmetic: a number or, for an
    /// associative array, the text itself. What a reference to an array
    /// element can hold.
    fn literal_key(&self, name: &str, index: &str) -> Key {
        if self.is_assoc(name) {
            return Key::Name(index.to_owned());
        }
        Key::Index(index.trim().parse().unwrap_or(0))
    }

    /// The attributes of `name`, if it was declared.
    #[must_use]
    pub fn attrs(&self, name: &str) -> Option<Attrs> {
        self.attrs.get(name).copied()
    }

    /// Declares `name`, adding `add` to its attributes and taking away
    /// `remove`. `-a`/`-A` turn an unset name into an empty array.
    ///
    /// # Errors
    ///
    /// Returns an [`AssignError`] when the array kinds do not fit together.
    pub fn declare(&mut self, name: &str, add: Attrs, remove: Attrs) -> Result<(), AssignError> {
        let attrs = self.attrs.get(name).copied().unwrap_or_default();
        let mut attrs = attrs.union(add).without(remove);
        if attrs.contains(Attrs::LOWER) && add.contains(Attrs::UPPER) {
            attrs = attrs.without(Attrs::LOWER);
        } else if attrs.contains(Attrs::UPPER) && add.contains(Attrs::LOWER) {
            attrs = attrs.without(Attrs::UPPER);
        }
        if add.contains(Attrs::ASSOC) {
            let convertible = self
                .arrays
                .get(name)
                .map_or_else(|| !self.vars.contains_key(name), Array::is_assoc);
            if !convertible {
                return Err(AssignError::Conversion(name.to_owned()));
            }
        }
        if add.contains(Attrs::INDEXED) && self.is_assoc(name) {
            return Err(AssignError::Conversion(name.to_owned()));
        }
        self.attrs.insert(name.to_owned(), attrs);
        if add.contains(Attrs::INDEXED) && self.vars.contains_key(name) {
            self.array_entry(name)?;
        }
        Ok(())
    }

    /// Every array variable, sorted by name.
    #[must_use]
    pub fn all_arrays(&self) -> Vec<(&str, &Array)> {
        let mut all: Vec<_> = self.arrays.iter().map(|(k, v)| (k.as_str(), v)).collect();
        all.sort_unstable_by_key(|(k, _)| *k);
        all
    }

    /// Every declared name, set or not, sorted.
    #[must_use]
    pub fn declared_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self
            .vars
            .keys()
            .chain(self.arrays.keys())
            .chain(self.attrs.keys())
            .chain(self.readonly.iter())
            .chain(self.exported.iter())
            .map(String::as_str)
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    }

    /// Whether `name` is exported.
    #[must_use]
    pub fn is_exported(&self, name: &str) -> bool {
        self.exported.contains(name)
    }

    #[must_use]
    pub fn is_readonly(&self, name: &str) -> bool {
        self.readonly.contains(name)
    }

    pub fn mark_readonly(&mut self, name: &str) {
        self.readonly.insert(name.to_owned());
    }

    /// Readonly variables and their values, sorted by name.
    #[must_use]
    pub fn readonly_vars(&self) -> Vec<(String, Option<String>)> {
        let mut vars: Vec<_> = self
            .readonly
            .iter()
            .map(|name| (name.clone(), self.vars.get(name).cloned()))
            .collect();
        vars.sort();
        vars
    }

    // ------------------------------------------------------------------
    // Function scopes (`local`)
    // ------------------------------------------------------------------

    pub fn push_scope(&mut self) {
        self.local_frames.push(Vec::new());
    }

    /// Ends the innermost function scope, putting back every variable a
    /// `local` in it shadowed.
    pub fn pop_scope(&mut self) {
        let Some(frame) = self.local_frames.pop() else {
            return;
        };
        for saved in frame.into_iter().rev() {
            self.unset_ref(&saved.name);
            if let Some(attrs) = saved.attrs {
                self.attrs.insert(saved.name.clone(), attrs);
            }
            match saved.value {
                Some(Value::Scalar(value)) => {
                    if saved.exported {
                        self.exported.insert(saved.name.clone());
                        set_process_var(&saved.name, &value);
                    }
                    self.vars.insert(saved.name.clone(), value);
                    self.var_changed(&saved.name);
                }
                Some(Value::Array(array)) => {
                    if saved.exported {
                        self.exported.insert(saved.name.clone());
                    }
                    self.arrays.insert(saved.name, array);
                }
                None => {}
            }
        }
    }

    /// Makes `name` local to the innermost function scope, keeping its
    /// current value until assigned. Returns false outside a function.
    pub fn make_local(&mut self, name: &str) -> bool {
        let Some(frame) = self.local_frames.last_mut() else {
            return false;
        };
        if !frame.iter().any(|s| s.name == name) {
            let value = self.vars.get(name).map_or_else(
                || self.arrays.get(name).cloned().map(Value::Array),
                |v| Some(Value::Scalar(v.clone())),
            );
            frame.push(SavedVar {
                name: name.to_owned(),
                value,
                exported: self.exported.contains(name),
                attrs: self.attrs.get(name).copied(),
            });
        }
        true
    }

    // ------------------------------------------------------------------
    // Command hash (`hash`)
    // ------------------------------------------------------------------

    #[must_use]
    pub fn hashed_command(&self, name: &str) -> Option<&str> {
        self.command_hash.get(name).map(String::as_str)
    }

    pub fn hash_command(&mut self, name: &str, path: String) {
        self.command_hash.insert(name.to_owned(), path);
    }

    pub fn clear_command_hash(&mut self) {
        self.command_hash.clear();
    }

    /// Hashed commands and their paths, sorted by name.
    #[must_use]
    pub fn hashed_commands(&self) -> Vec<(String, String)> {
        let mut all: Vec<_> = self
            .command_hash
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        all.sort();
        all
    }

    pub fn export(&mut self, name: impl Into<String>, value: impl Into<String>) {
        let name = name.into();
        let value = value.into();
        if name == "PATH" {
            self.command_hash.clear();
        }
        set_process_var(&name, &value);
        self.exported.insert(name.clone());
        self.vars.insert(name, value);
    }

    pub fn mark_exported(&mut self, name: &str) {
        self.exported.insert(name.to_owned());
        // An array cannot be passed on in the environment.
        if self.arrays.contains_key(name) {
            return;
        }
        let value = self.vars.entry(name.to_owned()).or_default().clone();
        set_process_var(name, &value);
    }

    /// `export -n name`: keeps the shell variable but drops its export
    /// attribute, so children no longer inherit it.
    pub fn unexport(&mut self, name: &str) {
        self.exported.remove(name);
        remove_process_var(name);
    }

    /// Removes a variable (and its export/environment-variable state) only,
    /// leaving a same-named function untouched.
    pub fn unset_var(&mut self, name: &str) {
        if let Some(target) = self.nameref_target(name) {
            self.unset_var(base_name(&target));
            return;
        }
        self.unset_ref(name);
    }

    /// `unset -n name`: removes `name` itself, a reference or not.
    pub fn unset_ref(&mut self, name: &str) {
        self.vars.remove(name);
        self.arrays.remove(name);
        self.attrs.remove(name);
        self.exported.remove(name);
        remove_process_var(name);
        self.var_changed(name);
    }

    /// Removes a function only, leaving a same-named variable untouched.
    pub fn unset_function(&mut self, name: &str) {
        self.functions.remove(name);
    }

    /// `unset name` with neither `-v` nor `-f`: POSIX has this act on a
    /// variable if one exists, falling back to a same-named function only
    /// when it doesn't (rather than removing both indiscriminately).
    pub fn unset(&mut self, name: &str) {
        if self.vars.contains_key(name)
            || self.arrays.contains_key(name)
            || self.attrs.contains_key(name)
        {
            self.unset_var(name);
        } else {
            self.unset_function(name);
        }
    }

    pub fn exported(&self) -> impl Iterator<Item = (&str, &str)> {
        self.exported
            .iter()
            .filter_map(|k| self.vars.get(k).map(|v| (k.as_str(), v.as_str())))
    }

    pub fn all_vars(&self) -> impl Iterator<Item = (&str, &str)> {
        self.vars.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    // ------------------------------------------------------------------
    // Functions
    // ------------------------------------------------------------------

    /// Defines function `name`, read from file `source` (for
    /// `BASH_SOURCE`; empty when it was not read from a file).
    pub fn define_function(&mut self, name: String, body: Command, source: Rc<str>) {
        self.function_sources.insert(name.clone(), source);
        self.functions.insert(name, Rc::new(body));
    }

    /// The file function `name` was defined in.
    #[must_use]
    pub fn function_source(&self, name: &str) -> Option<Rc<str>> {
        self.function_sources.get(name).cloned()
    }
    /// Every function's name, sorted.
    #[must_use]
    pub fn function_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.functions.keys().map(String::as_str).collect();
        names.sort_unstable();
        names
    }

    /// A function is running, so `local` has a scope to put variables in.
    #[must_use]
    pub const fn in_function(&self) -> bool {
        !self.local_frames.is_empty()
    }

    /// Returns a cheap `Rc` clone of the function body rather than `&Command`:
    /// callers need an owned value to run while `self` is mutably borrowed
    /// for the call, and an `Rc` clone (a refcount bump) is what makes that
    /// affordable on every invocation instead of deep-cloning the AST.
    #[must_use]
    pub fn get_function(&self, name: &str) -> Option<Rc<Command>> {
        self.functions.get(name).cloned()
    }

    // ------------------------------------------------------------------
    // Aliases
    // ------------------------------------------------------------------

    /// The current alias definitions, for the parser.
    #[must_use]
    pub fn aliases(&self) -> Rc<AliasMap> {
        if self.expand_aliases {
            Rc::clone(&self.aliases)
        } else {
            Rc::default()
        }
    }

    pub fn set_alias(&mut self, name: String, value: String) {
        Rc::make_mut(&mut self.aliases).insert(name, value);
    }
    #[must_use]
    pub fn get_alias(&self, name: &str) -> Option<String> {
        self.aliases.get(name).cloned()
    }
    /// Removes an alias, reporting whether one by that name actually
    /// existed (`unalias` treats a nonexistent name as an error).
    pub fn remove_alias(&mut self, name: &str) -> bool {
        Rc::make_mut(&mut self.aliases).remove(name).is_some()
    }
    pub fn clear_aliases(&mut self) {
        Rc::make_mut(&mut self.aliases).clear();
    }
    pub fn all_aliases(&self) -> impl Iterator<Item = (&str, &str)> {
        self.aliases.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    // ------------------------------------------------------------------
    // Positional parameters
    // ------------------------------------------------------------------

    #[inline]
    #[must_use]
    pub fn positional_args(&self) -> &[String] {
        &self.positional
    }
    pub fn set_positional_args(&mut self, args: Vec<String>) {
        self.positional = args;
    }
    /// Sets the positional parameters, returning the old ones.
    pub const fn replace_positional_args(&mut self, args: Vec<String>) -> Vec<String> {
        std::mem::replace(&mut self.positional, args)
    }

    pub fn restore_vars(&mut self, saved: Vec<(String, Option<String>)>) {
        for (k, old) in saved {
            match old {
                Some(v) => self.set(&k, v),
                None => self.unset(&k),
            }
        }
    }
}

/// `name[index]` split into its name and subscript.
fn split_subscript(target: &str) -> Option<(&str, &str)> {
    let (name, rest) = target.split_once('[')?;
    Some((name, rest.strip_suffix(']')?))
}

/// The variable a reference's target names, without any subscript.
fn base_name(target: &str) -> &str {
    target.split_once('[').map_or(target, |(name, _)| name)
}
