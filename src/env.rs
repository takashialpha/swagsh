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

#[derive(Debug, Clone)]
pub struct Env {
    vars: HashMap<String, String>,
    exported: HashSet<String>,
    functions: HashMap<String, Rc<Command>>,
    aliases: HashMap<String, String>,
    positional: Vec<String>,
}

impl Env {
    pub fn from_process() -> Self {
        let raw: Vec<(String, String)> = std::env::vars().collect();
        let cap = raw.len() + 8;
        let mut vars = HashMap::with_capacity_and_hasher(cap, Default::default());
        let mut exported = HashSet::with_capacity_and_hasher(cap, Default::default());
        for (k, v) in raw {
            exported.insert(k.clone());
            vars.insert(k, v);
        }
        vars.entry("PATH".into())
            .or_insert_with(|| "/usr/local/bin:/usr/bin:/bin".into());
        vars.entry("HOME".into())
            .or_insert_with(|| std::env::var("HOME").unwrap_or_else(|_| "/".into()));
        vars.entry("IFS".into()).or_insert_with(|| " \t\n".into());
        // Preserve the inherited $PWD (logical path through symlinks set by the
        // parent shell). Only fall back to the kernel cwd if $PWD is absent.
        if vars.contains_key("PWD") {
            exported.insert("PWD".into());
        } else if let Ok(cwd) = std::env::current_dir() {
            let s = cwd.to_string_lossy().into_owned();
            exported.insert("PWD".into());
            vars.insert("PWD".into(), s);
        }
        Self {
            vars,
            exported,
            functions: HashMap::default(),
            aliases: HashMap::default(),
            positional: Vec::new(),
        }
    }

    // ------------------------------------------------------------------
    // Variables
    // ------------------------------------------------------------------

    #[inline]
    pub fn get(&self, name: &str) -> Option<String> {
        self.vars.get(name).cloned()
    }
    #[inline]
    pub fn get_or_empty(&self, name: &str) -> String {
        self.vars.get(name).cloned().unwrap_or_default()
    }

    pub fn set(&mut self, name: &str, value: impl Into<String>) {
        let value = value.into();
        if self.exported.contains(name) {
            // SAFETY: single-threaded shell: no concurrent env mutation.
            unsafe {
                std::env::set_var(name, &value);
            }
        }
        self.vars.insert(name.to_owned(), value);
    }

    pub fn export(&mut self, name: impl Into<String>, value: impl Into<String>) {
        let name = name.into();
        let value = value.into();
        // SAFETY: single-threaded shell: no concurrent env mutation.
        unsafe {
            std::env::set_var(&name, &value);
        }
        self.exported.insert(name.clone());
        self.vars.insert(name, value);
    }

    pub fn mark_exported(&mut self, name: &str) {
        let value = self.vars.entry(name.to_owned()).or_default().clone();
        // SAFETY: single-threaded shell: no concurrent env mutation.
        unsafe {
            std::env::set_var(name, &value);
        }
        self.exported.insert(name.to_owned());
    }

    /// `export -n name`: keeps the shell variable but drops its export
    /// attribute, so children no longer inherit it.
    pub fn unexport(&mut self, name: &str) {
        self.exported.remove(name);
        // SAFETY: single-threaded shell: no concurrent env mutation.
        unsafe {
            std::env::remove_var(name);
        }
    }

    /// Removes a variable (and its export/environment-variable state) only,
    /// leaving a same-named function untouched.
    pub fn unset_var(&mut self, name: &str) {
        self.vars.remove(name);
        self.exported.remove(name);
        // SAFETY: single-threaded shell: no concurrent env mutation.
        unsafe {
            std::env::remove_var(name);
        }
    }

    /// Removes a function only, leaving a same-named variable untouched.
    pub fn unset_function(&mut self, name: &str) {
        self.functions.remove(name);
    }

    /// `unset name` with neither `-v` nor `-f`: POSIX has this act on a
    /// variable if one exists, falling back to a same-named function only
    /// when it doesn't (rather than removing both indiscriminately).
    pub fn unset(&mut self, name: &str) {
        if self.vars.contains_key(name) {
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

    pub fn define_function(&mut self, name: String, body: Command) {
        self.functions.insert(name, Rc::new(body));
    }
    /// Returns a cheap `Rc` clone of the function body rather than `&Command`:
    /// callers need an owned value to run while `self` is mutably borrowed
    /// for the call, and an `Rc` clone (a refcount bump) is what makes that
    /// affordable on every invocation instead of deep-cloning the AST.
    pub fn get_function(&self, name: &str) -> Option<Rc<Command>> {
        self.functions.get(name).cloned()
    }

    // ------------------------------------------------------------------
    // Aliases
    // ------------------------------------------------------------------

    pub fn set_alias(&mut self, name: String, value: String) {
        self.aliases.insert(name, value);
    }
    pub fn get_alias(&self, name: &str) -> Option<String> {
        self.aliases.get(name).cloned()
    }
    /// Removes an alias, reporting whether one by that name actually
    /// existed (`unalias` treats a nonexistent name as an error).
    pub fn remove_alias(&mut self, name: &str) -> bool {
        self.aliases.remove(name).is_some()
    }
    pub fn clear_aliases(&mut self) {
        self.aliases.clear();
    }
    pub fn all_aliases(&self) -> impl Iterator<Item = (&str, &str)> {
        self.aliases.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    // ------------------------------------------------------------------
    // Positional parameters
    // ------------------------------------------------------------------

    #[inline]
    pub fn positional_args(&self) -> &[String] {
        &self.positional
    }
    pub fn set_positional_args(&mut self, args: Vec<String>) {
        self.positional = args;
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
