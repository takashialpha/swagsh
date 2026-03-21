const APP_NAME: &str = "swagsh";

use clap::Parser;
use std::path::PathBuf;

/// Command line interface for the shell.
#[derive(Debug, Parser, Clone)]
#[command(
    name = APP_NAME,
    version,
    about = "A sleek, high-performance POSIX-compatible shell written in Rust.",
    long_about = "A sleek, high-performance POSIX-compatible shell written in Rust \
for speed, reliability, and modern system integration."
)]
pub struct Cli {
    /// Do not read configuration files
    #[arg(short = 'N', long = "no-config")]
    pub no_config: bool,

    /// Check syntax but do not execute commands
    #[arg(short = 'n', long = "no-execute")]
    pub no_execute: bool,

    /// Enable private mode (history will not be read or written)
    #[arg(short = 'P', long = "private")]
    pub private: bool,

    /// Execute a command string
    #[arg(
        short = 'c',
        value_name = "CMD",
        help = "Execute the given command string",
        conflicts_with = "script"
    )]
    pub command: Option<String>,

    /// Script file to execute
    #[arg(value_name = "SCRIPT", help = "Script file to execute")]
    pub script: Option<PathBuf>,

    /// Arguments passed to the script
    #[arg(value_name = "ARGS")]
    pub args: Vec<String>,
}

impl Cli {
    /// Detect whether the shell was started as a login shell.
    /// This happens when argv[0] starts with `-`, which is how
    /// system login programs traditionally launch shells.
    pub fn login_shell(argv0: &str) -> bool {
        argv0.starts_with('-')
    }
}
