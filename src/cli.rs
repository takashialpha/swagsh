use clap::Parser;

/// A fast, minimal, modern Linux shell. Named after swag, slang for stylish flair.
// CLI flag structs legitimately hold many booleans; grouping them would add
// indirection without any benefit.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Parser, Clone)]
#[command(name = "swagsh", version)]
pub struct Cli {
    /// Execute CMD then exit. The first remaining positional becomes $0, the
    /// rest $1, $2, ...
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

    /// Start as a login shell, sourcing `~/.swagsh_profile` instead of `~/.swagshrc`.
    #[arg(short = 'l', long = "login")]
    pub login: bool,

    /// Read commands from standard input, even with ARGS: they all become
    /// $1, $2, ...
    #[arg(short = 's')]
    pub stdin: bool,

    /// Run interactively, even when standard input or error is not a terminal.
    #[arg(short = 'i')]
    pub interactive: bool,

    /// Without -c: first positional is the script, the rest become $1, $2, ...
    /// With -c: the first becomes $0, the rest $1, $2, ...
    #[arg(value_name = "ARGS")]
    pub positionals: Vec<String>,
}

impl Cli {
    /// Returns `(script_path, positional_args)` split from `positionals`.
    #[must_use]
    pub fn split_positionals(&self) -> (Option<std::path::PathBuf>, Vec<String>) {
        if self.command.is_some() || self.stdin || self.positionals.is_empty() {
            (None, self.positionals.clone())
        } else {
            (
                Some(std::path::PathBuf::from(&self.positionals[0])),
                self.positionals[1..].to_vec(),
            )
        }
    }

    /// True if `argv[0]` starts with `-`, the convention used by login programs.
    #[must_use]
    pub fn is_login_shell(argv0: &str) -> bool {
        argv0.starts_with('-')
    }
}

/// The letters of the `set` options that may be given on the command line.
const SET_LETTERS: &str = "abCefHmnuvx";

/// Splits the shell's arguments (without `argv[0]`) into those for [`Cli`]
/// and the `set` options among them (`-e`, `+x`, `-o pipefail`).
///
/// A cluster can mix the two (`-ec`). Options end at the first
/// operand, the script or the `-c` command, so that the arguments after it
/// are the script's even when they start with `-`.
#[must_use]
pub fn split_set_options(args: impl IntoIterator<Item = String>) -> (Vec<String>, Vec<String>) {
    let mut cli = Vec::new();
    let mut set = Vec::new();
    let mut args = args.into_iter();
    let mut command = false;
    while let Some(arg) = args.next() {
        if arg == "-" || arg == "--" {
            break;
        }
        if arg.starts_with("--") {
            cli.push(arg);
            continue;
        }
        let Some(sign) = arg
            .chars()
            .next()
            .filter(|&c| (c == '-' || c == '+') && arg.len() > 1)
        else {
            push_operands(&mut cli, command, std::iter::once(arg).chain(args));
            return (cli, set);
        };
        let own = if sign == '-' { "csil" } else { "" };
        let letters = &arg[1..];
        if !letters
            .chars()
            .all(|c| SET_LETTERS.contains(c) || c == 'o' || own.contains(c))
        {
            cli.push(arg);
            continue;
        }
        for c in letters.chars() {
            if c == 'c' {
                command = true;
            } else if own.contains(c) {
                cli.push(format!("-{c}"));
            } else if c == 'o' {
                set.push(format!("{sign}o"));
                set.extend(args.next());
            } else {
                set.push(format!("{sign}{c}"));
            }
        }
    }
    push_operands(&mut cli, command, args);
    (cli, set)
}

/// Adds the operands to the arguments for [`Cli`]: the first is `-c`'s
/// command string when `command` is set.
fn push_operands(cli: &mut Vec<String>, command: bool, operands: impl IntoIterator<Item = String>) {
    let mut operands = operands.into_iter();
    if command {
        cli.push("-c".to_owned());
        cli.extend(operands.next());
    }
    cli.push("--".to_owned());
    cli.extend(operands);
}
