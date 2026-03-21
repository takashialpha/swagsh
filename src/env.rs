use crate::ast::Command;
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Env — the shell's variable and function store
// ---------------------------------------------------------------------------

/// Holds all shell state that persists across commands: variables, their
/// export status, shell functions, and positional parameters.
#[derive(Debug, Clone)]
pub struct Env {
    /// All variables (exported or not). Values are plain strings.
    vars: HashMap<String, String>,
    /// Set of variable names that are marked for export to child processes.
    exported: std::collections::HashSet<String>,
    /// Shell functions defined via `name() { ... }` or `function name`.
    functions: HashMap<String, Command>,
    /// Positional parameters: $1, $2, … ($0 is handled separately).
    positional: Vec<String>,
}

impl Env {
    /// Build an `Env` pre-populated from the process environment.
    pub fn from_process() -> Self {
        let mut vars = HashMap::new();
        let mut exported = std::collections::HashSet::new();

        for (k, v) in std::env::vars() {
            exported.insert(k.clone());
            vars.insert(k, v);
        }

        // Ensure sensible defaults if the parent didn't provide them.
        vars.entry("PATH".into())
            .or_insert_with(|| "/usr/local/bin:/usr/bin:/bin".into());
        vars.entry("HOME".into())
            .or_insert_with(|| std::env::var("HOME").unwrap_or_else(|_| "/".into()));
        vars.entry("IFS".into()).or_insert_with(|| " \t\n".into());

        // Initialise PWD from the real cwd, not whatever the parent exported.
        if let Ok(cwd) = std::env::current_dir() {
            vars.insert("PWD".into(), cwd.to_string_lossy().into_owned());
            exported.insert("PWD".into());
        }

        Self {
            vars,
            exported,
            functions: HashMap::new(),
            positional: Vec::new(),
        }
    }

    // ------------------------------------------------------------------
    // Variable access
    // ------------------------------------------------------------------

    /// Get the value of a variable, returning `None` if unset.
    pub fn get(&self, name: &str) -> Option<String> {
        self.vars.get(name).cloned()
    }

    /// Get the value of a variable, returning an empty string if unset.
    pub fn get_or_empty(&self, name: &str) -> String {
        self.vars.get(name).cloned().unwrap_or_default()
    }

    /// Set a variable without exporting it.
    pub fn set(&mut self, name: &str, value: impl Into<String>) {
        let value = value.into();
        // Propagate to the real process environment if it's already exported,
        // so child processes spawned via execvp see the updated value.
        if self.exported.contains(name) {
            // SAFETY: single-threaded shell process — no concurrent env mutation.
            unsafe {
                std::env::set_var(name, &value);
            }
        }
        self.vars.insert(name.to_owned(), value);
    }

    /// Set a variable and immediately mark it as exported.
    pub fn export(&mut self, name: impl Into<String>, value: impl Into<String>) {
        let name = name.into();
        let value = value.into();
        // SAFETY: single-threaded shell process — no concurrent env mutation.
        unsafe {
            std::env::set_var(&name, &value);
        }
        self.exported.insert(name.clone());
        self.vars.insert(name, value);
    }

    /// Mark an existing variable as exported without changing its value.
    /// If the variable doesn't exist yet it is created as an empty string.
    pub fn mark_exported(&mut self, name: &str) {
        let value = self.vars.entry(name.to_owned()).or_default().clone();
        // SAFETY: single-threaded shell process — no concurrent env mutation.
        unsafe {
            std::env::set_var(name, &value);
        }
        self.exported.insert(name.to_owned());
    }

    /// Remove a variable entirely (both value and export flag).
    pub fn unset(&mut self, name: &str) {
        self.vars.remove(name);
        self.exported.remove(name);
        self.functions.remove(name);
        // SAFETY: single-threaded shell process — no concurrent env mutation.
        unsafe {
            std::env::remove_var(name);
        }
    }

    /// Iterator over all exported `(name, value)` pairs — used by `export`
    /// with no arguments and by the executor when printing the environment.
    pub fn exported(&self) -> impl Iterator<Item = (&str, &str)> {
        self.exported
            .iter()
            .filter_map(|k| self.vars.get(k).map(|v| (k.as_str(), v.as_str())))
    }

    /// Iterator over every variable regardless of export status — used by
    /// `set` with no arguments.
    pub fn all_vars(&self) -> impl Iterator<Item = (&str, &str)> {
        self.vars.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    // ------------------------------------------------------------------
    // Shell functions
    // ------------------------------------------------------------------

    /// Define or redefine a shell function.
    pub fn define_function(&mut self, name: String, body: Command) {
        self.functions.insert(name, body);
    }

    /// Look up a shell function by name.
    pub fn get_function(&self, name: &str) -> Option<&Command> {
        self.functions.get(name)
    }

    // ------------------------------------------------------------------
    // Positional parameters
    // ------------------------------------------------------------------

    /// Read-only view of the current positional parameters ($1 … $N).
    pub fn positional_args(&self) -> &[String] {
        &self.positional
    }

    /// Replace the positional parameters wholesale (used by `set --` and
    /// function call/return).
    pub fn set_positional_args(&mut self, args: Vec<String>) {
        self.positional = args;
    }
}
