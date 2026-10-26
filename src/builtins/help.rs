//! `help`: what each builtin does, and how to call it.
//!
//! Every builtin has a [`Help`] page. One taking its flags through clap
//! gets it from its clap command, with any examples and notes its
//! [`Builtin`] impl adds; a hand-parsed one writes its own.

use std::fmt::Write as _;

use anyhow::Result;
use clap::Parser;

use super::{BUILTINS, Builtin, BuiltinFailed};
use crate::errfmt::emit;
use crate::eval::Shell;
use crate::expand::glob_match;
use crate::jobs::ExitStatus;

/// A command line showing a builtin in use, and what it does.
pub struct Example {
    pub command: &'static str,
    pub meaning: &'static str,
}

/// What `help NAME` shows.
pub struct HelpPage {
    /// One line on what the builtin does.
    pub summary: String,
    /// The synopsis, without the `Usage: ` before it.
    pub usage: String,
    /// The sections on its operands and options, laid out as clap does.
    pub body: String,
    /// Its flags (`-r`, `-o OPTION`) and what each does, for completion.
    pub options: Vec<(String, String)>,
    pub examples: &'static [Example],
    pub notes: &'static [&'static str],
}

impl HelpPage {
    /// A page for a hand-parsed builtin: `options` are the flags and what
    /// each does.
    #[must_use]
    pub fn manual(
        summary: &str,
        usage: &str,
        options: &[(&str, &str)],
        examples: &'static [Example],
        notes: &'static [&'static str],
    ) -> Self {
        Self {
            summary: summary.to_owned(),
            usage: usage.to_owned(),
            body: section("Options", options),
            options: options
                .iter()
                .map(|&(flag, what)| (flag.to_owned(), what.to_owned()))
                .collect(),
            examples,
            notes,
        }
    }

    /// The whole page, as `help NAME` prints it.
    fn render(&self) -> String {
        let mut out = format!("{}\n\nUsage: {}\n", self.summary, self.usage);
        if !self.body.is_empty() {
            out.push('\n');
            out.push_str(&self.body);
        }
        if !self.examples.is_empty() {
            let pairs: Vec<(&str, &str)> = self
                .examples
                .iter()
                .map(|e| (e.command, e.meaning))
                .collect();
            out.push('\n');
            out.push_str(&section("Examples", &pairs));
        }
        if !self.notes.is_empty() {
            out.push_str("\nNotes:\n");
            for note in self.notes {
                out.push_str("  ");
                out.push_str(note);
                out.push('\n');
            }
        }
        out
    }
}

/// A titled list of `(term, meaning)` pairs, the meanings lined up.
pub(super) fn section(title: &str, items: &[(&str, &str)]) -> String {
    if items.is_empty() {
        return String::new();
    }
    let width = items
        .iter()
        .map(|(term, _)| term.chars().count())
        .max()
        .unwrap_or(0);
    let mut out = format!("{title}:\n");
    for (term, meaning) in items {
        let line = format!("  {term:<width$}  {meaning}");
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// A builtin's help page.
pub trait Help {
    fn help() -> HelpPage;
}

impl<T: Builtin> Help for T {
    fn help() -> HelpPage {
        clap_page::<T>(T::examples(), T::notes())
    }
}

/// The page clap's command for `T` describes, with `examples` and `notes`.
pub fn clap_page<T: Parser>(
    examples: &'static [Example],
    notes: &'static [&'static str],
) -> HelpPage {
    let mut command = super::cli::command_for::<T>();
    let options = command
        .get_arguments()
        .filter_map(|arg| {
            let flag = arg
                .get_short()
                .map(|c| format!("-{c}"))
                .or_else(|| arg.get_long().map(|l| format!("--{l}")))?;
            Some((
                flag,
                arg.get_help().map(ToString::to_string).unwrap_or_default(),
            ))
        })
        .collect();
    let summary = command
        .get_about()
        .map(ToString::to_string)
        .unwrap_or_default();
    let usage = command.render_usage().to_string();
    let usage = usage.strip_prefix("Usage: ").unwrap_or(&usage).to_owned();
    let mut body = command
        .help_template("{all-args}")
        .render_help()
        .to_string()
        .trim_end()
        .to_owned();
    if !body.is_empty() {
        body.push('\n');
    }
    HelpPage {
        summary,
        usage,
        body,
        options,
        examples,
        notes,
    }
}

/// A builtin that parses its own arguments, rather than through clap.
pub trait HandParsed: Help {
    /// Whether a lone `--help` shows the help page. Not for a builtin that
    /// could be given `--help` as an operand (`echo --help` prints it).
    const HELP_FLAG: bool = true;

    /// # Errors
    ///
    /// Returns an error if the builtin fails.
    fn run(shell: &mut Shell, args: &[&str]) -> Result<ExitStatus>;
}

/// Runs hand-parsed builtin `H`, or shows its page for `--help`.
///
/// # Errors
///
/// Returns an error if the builtin fails.
pub fn hand_parsed<H: HandParsed>(shell: &mut Shell, args: &[&str]) -> Result<ExitStatus> {
    if H::HELP_FLAG && args == ["--help"] {
        shell.out(&H::help().render())?;
        return Ok(ExitStatus::SUCCESS);
    }
    H::run(shell, args)
}

/// [`Help::help`] as a plain function, for the builtins table.
#[must_use]
pub fn page<T: Help>() -> HelpPage {
    T::help()
}

#[derive(Parser)]
#[command(name = "help", about = "Show what builtins do and how to call them")]
pub struct HelpBuiltin {
    /// Show only the usage line of each
    #[arg(short = 's')]
    usage: bool,
    /// Show only the one-line summary of each
    #[arg(short = 'd')]
    summary: bool,
    /// Builtins to show, as names or patterns (`help 'p*'`); all when none
    #[arg(value_name = "PATTERN")]
    patterns: Vec<String>,
}

impl Builtin for HelpBuiltin {
    fn run(self, shell: &mut Shell) -> Result<ExitStatus> {
        if self.patterns.is_empty() {
            return list(shell, self.usage);
        }
        let mut status = ExitStatus::SUCCESS;
        let mut shown = 0;
        for pattern in &self.patterns {
            let matching: Vec<_> = BUILTINS
                .iter()
                .filter(|(name, ..)| glob_match(pattern, name))
                .collect();
            if matching.is_empty() {
                emit(format!(
                    "help: no help topics match `{pattern}'; try `man {pattern}'"
                ));
                status = ExitStatus(1);
                continue;
            }
            for (name, _, page) in matching {
                let page = page();
                let text = if self.usage {
                    format!("{name}: {}\n", page.usage)
                } else if self.summary {
                    format!("{name} - {}\n", page.summary)
                } else if shown == 0 {
                    page.render()
                } else {
                    format!("\n{}", page.render())
                };
                shell.out(&text)?;
                shown += 1;
            }
        }
        if status.is_success() {
            Ok(status)
        } else {
            Err(BuiltinFailed(status).into())
        }
    }

    fn examples() -> &'static [Example] {
        &[
            Example {
                command: "help",
                meaning: "List every builtin with what it does",
            },
            Example {
                command: "help cd",
                meaning: "Show how to call cd",
            },
            Example {
                command: "help -s 'p*'",
                meaning: "The usage of every builtin starting with p",
            },
        ]
    }

    fn notes() -> &'static [&'static str] {
        &[
            "Every builtin also takes `--help`, except those whose operands could be",
            "`--help` itself (`echo`, `test`, `[`, `:`, `true`, `false`).",
            "For a command that is not a builtin, see its manual page with `man`.",
        ]
    }
}

/// Every builtin with its summary, or with `usage`, its usage line.
fn list(shell: &mut Shell, usage: bool) -> Result<ExitStatus> {
    let width = BUILTINS
        .iter()
        .map(|(name, ..)| name.len())
        .max()
        .unwrap_or(0);
    let mut out = format!(
        "swagsh {} builtins. `help NAME` shows one in full.\n\n",
        env!("CARGO_PKG_VERSION")
    );
    for (name, _, page) in BUILTINS {
        let page = page();
        let text = if usage { page.usage } else { page.summary };
        let _ = writeln!(out, "  {name:<width$}  {text}");
    }
    shell.out(&out)?;
    Ok(ExitStatus::SUCCESS)
}
