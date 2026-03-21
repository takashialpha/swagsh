use ahash::AHashMap;
use ahash::AHashSet;

use crate::ast::Command;

// ---------------------------------------------------------------------------
// Env — the shell's variable and function store
// ---------------------------------------------------------------------------

/// Uses `AHashMap` (non-cryptographic, ~2× faster than `std::HashMap` for
/// short string keys) for both the variable store and the exported set.
#[derive(Debug, Clone)]
pub struct Env {
    vars: AHashMap<String, String>,
    exported: AHashSet<String>,
    functions: AHashMap<String, Command>,
    positional: Vec<String>,
}

impl Env {
    /// Build an `Env` pre-populated from the process environment.
    /// Pre-allocates to avoid rehash growth steps during population.
    pub fn from_process() -> Self {
        let raw: Vec<(String, String)> = std::env::vars().collect();
        let cap = raw.len() + 8;

        let mut vars = AHashMap::with_capacity(cap);
        let mut exported = AHashSet::with_capacity(cap);

        for (k, v) in raw {
            exported.insert(k.clone());
            vars.insert(k, v);
        }

        vars.entry("PATH".into())
            .or_insert_with(|| "/usr/local/bin:/usr/bin:/bin".into());
        vars.entry("HOME".into())
            .or_insert_with(|| std::env::var("HOME").unwrap_or_else(|_| "/".into()));
        vars.entry("IFS".into()).or_insert_with(|| " \t\n".into());

        if let Ok(cwd) = std::env::current_dir() {
            let s = cwd.to_string_lossy().into_owned();
            vars.insert("PWD".into(), s);
            exported.insert("PWD".into());
        }

        Self {
            vars,
            exported,
            functions: AHashMap::new(),
            positional: Vec::new(),
        }
    }

    // ------------------------------------------------------------------
    // Variable access
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
            // SAFETY: single-threaded shell — no concurrent env mutation.
            unsafe {
                std::env::set_var(name, &value);
            }
        }
        self.vars.insert(name.to_owned(), value);
    }

    pub fn export(&mut self, name: impl Into<String>, value: impl Into<String>) {
        let name = name.into();
        let value = value.into();
        // SAFETY: single-threaded shell — no concurrent env mutation.
        unsafe {
            std::env::set_var(&name, &value);
        }
        self.exported.insert(name.clone());
        self.vars.insert(name, value);
    }

    pub fn mark_exported(&mut self, name: &str) {
        let value = self.vars.entry(name.to_owned()).or_default().clone();
        // SAFETY: single-threaded shell — no concurrent env mutation.
        unsafe {
            std::env::set_var(name, &value);
        }
        self.exported.insert(name.to_owned());
    }

    pub fn unset(&mut self, name: &str) {
        self.vars.remove(name);
        self.exported.remove(name);
        self.functions.remove(name);
        // SAFETY: single-threaded shell — no concurrent env mutation.
        unsafe {
            std::env::remove_var(name);
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
    // Shell functions
    // ------------------------------------------------------------------

    pub fn define_function(&mut self, name: String, body: Command) {
        self.functions.insert(name, body);
    }

    pub fn get_function(&self, name: &str) -> Option<&Command> {
        self.functions.get(name)
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
}
