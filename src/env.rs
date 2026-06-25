use std::collections::{HashMap, HashSet};

use crate::ast::Command;

#[derive(Debug, Clone)]
pub struct Env {
    vars: HashMap<String, String>,
    exported: HashSet<String>,
    functions: HashMap<String, Command>,
    aliases: HashMap<String, String>,
    positional: Vec<String>,
}

impl Env {
    pub fn from_process() -> Self {
        let raw: Vec<(String, String)> = std::env::vars().collect();
        let cap = raw.len() + 8;
        let mut vars = HashMap::with_capacity(cap);
        let mut exported = HashSet::with_capacity(cap);
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
            functions: HashMap::new(),
            aliases: HashMap::new(),
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

    pub fn unset(&mut self, name: &str) {
        self.vars.remove(name);
        self.exported.remove(name);
        self.functions.remove(name);
        // SAFETY: single-threaded shell: no concurrent env mutation.
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
    // Functions
    // ------------------------------------------------------------------

    pub fn define_function(&mut self, name: String, body: Command) {
        self.functions.insert(name, body);
    }
    pub fn get_function(&self, name: &str) -> Option<&Command> {
        self.functions.get(name)
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
    pub fn remove_alias(&mut self, name: &str) {
        self.aliases.remove(name);
    }
    pub fn clear_aliases(&mut self) {
        self.aliases.clear();
    }
    pub fn all_aliases(&self) -> impl Iterator<Item = (&str, &str)> {
        self.aliases.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }
    /// All alias names: used by tab completion.
    pub fn alias_names(&self) -> impl Iterator<Item = &str> {
        self.aliases.keys().map(std::string::String::as_str)
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
