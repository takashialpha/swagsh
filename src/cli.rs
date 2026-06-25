use clap::Parser;

/// A fast, minimal, modern Linux shell. Named after swag, slang for stylish flair.
// CLI flag structs legitimately hold many booleans; grouping them would add
// indirection without any benefit.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Parser, Clone)]
#[command(name = "swagsh", version)]
pub struct Cli {
    /// Execute CMD then exit. Remaining positionals become $1, $2, ...
    #[arg(short = 'c', value_name = "CMD")]
    pub command: Option<String>,

    /// Parse and check syntax without executing anything.
    #[arg(long = "dry-run")]
    pub no_execute: bool,

    /// Skip sourcing startup files (only affects interactive mode).
    #[arg(long = "no-rc")]
    pub no_rc: bool,

    /// Do not read or write history.
    #[arg(long = "private")]
    pub private: bool,

    /// Start as a login shell, sourcing `~/.swagsh_profile`.
    #[arg(short = 'l', long = "login")]
    pub login: bool,

    /// Without -c: first positional is the script, the rest become $1, $2, ...
    /// With -c: all positionals become $1, $2, ...
    #[arg(value_name = "ARGS")]
    pub positionals: Vec<String>,
}

impl Cli {
    /// Returns `(script_path, positional_args)` split from `positionals`.
    pub fn split_positionals(&self) -> (Option<std::path::PathBuf>, Vec<String>) {
        if self.command.is_some() || self.positionals.is_empty() {
            (None, self.positionals.clone())
        } else {
            (
                Some(std::path::PathBuf::from(&self.positionals[0])),
                self.positionals[1..].to_vec(),
            )
        }
    }

    /// True if argv[0] starts with `-`, the convention used by login programs.
    pub fn is_login_shell(argv0: &str) -> bool {
        argv0.starts_with('-')
    }
}
