//! `help`: what each builtin does, and how to call it.
//!
//! Every builtin has a [`Help`] page. One taking its flags through clap
//! gets it from its clap command, with any examples and notes its
//! [`Builtin`] impl adds; a hand-parsed one writes its own.

use std::fmt::Write as _;

use anyhow::Result;
use clap::Parser;
use clap::builder::StyledStr;
use clap::builder::styling::{AnsiColor, Style, Styles};

use super::{BUILTINS, Builtin, BuiltinFailed};
use crate::errfmt::emit;
use crate::eval::Shell;
use crate::expand::glob_match;
use crate::jobs::ExitStatus;

const HEADING: Style = AnsiColor::Green.on_default().bold();
const LITERAL: Style = AnsiColor::Cyan.on_default().bold();
const PLACEHOLDER: Style = AnsiColor::Yellow.on_default();

/// The colors of every help page and usage error: `help NAME` and
/// `NAME --help` look the same.
pub const STYLES: Styles = Styles::styled()
    .header(HEADING)
    .usage(HEADING)
    .literal(LITERAL)
    .placeholder(PLACEHOLDER)
    .error(AnsiColor::Red.on_default().bold())
    .valid(LITERAL)
    .invalid(AnsiColor::Yellow.on_default().bold());

/// Whether output to standard output should be in color: it is a
/// terminal, and `$NO_COLOR` is unset.
#[must_use]
pub fn color(shell: &Shell) -> bool {
    crate::paint::stdout(shell)
}

/// `text` in `style`.
fn paint(out: &mut StyledStr, style: Style, text: &str) {
    let _ = write!(out, "{style}{text}{style:#}");
}

/// A usage line in color: the command's name and flags as literals, the
/// operands as placeholders.
fn styled_usage(usage: &str) -> StyledStr {
    let mut out = StyledStr::new();
    let mut first = true;
    let mut rest = usage;
    while !rest.is_empty() {
        let len = rest
            .find(|c: char| " []|{}()".contains(c))
            .unwrap_or(rest.len());
        if len == 0 {
            let c = rest.chars().next().map_or(1, char::len_utf8);
            out.push_str(&rest[..c]);
            rest = &rest[c..];
            continue;
        }
        let word = &rest[..len];
        let bare = word.trim_end_matches('.');
        let placeholder = bare.starts_with('<')
            || (!bare.is_empty()
                && bare
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'));
        if first || word.starts_with('-') {
            paint(&mut out, LITERAL, word);
        } else if placeholder {
            paint(&mut out, PLACEHOLDER, word);
        } else {
            out.push_str(word);
        }
        first = false;
        rest = &rest[len..];
    }
    out
}

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
    pub body: StyledStr,
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

    /// The whole page, as `help NAME` and `NAME --help` print it; with
    /// `color`, in [`STYLES`].
    #[must_use]
    pub fn render(&self, color: bool) -> String {
        let mut out = StyledStr::new();
        paint(&mut out, Style::new().bold(), &self.summary);
        out.push_str("\n\n");
        paint(&mut out, HEADING, "Usage:");
        let _ = writeln!(out, " {}", styled_usage(&self.usage).ansi());
        let body = self.body.ansi().to_string();
        if !body.trim().is_empty() {
            let _ = writeln!(out, "\n{}", body.trim_end());
        }
        if !self.examples.is_empty() {
            let pairs: Vec<(&str, &str)> = self
                .examples
                .iter()
                .map(|e| (e.command, e.meaning))
                .collect();
            let _ = write!(out, "\n{}", section("Examples", &pairs).ansi());
        }
        if !self.notes.is_empty() {
            out.push_str("\n");
            paint(&mut out, HEADING, "Notes:");
            out.push_str("\n");
            for note in self.notes {
                let _ = writeln!(out, "  {note}");
            }
        }
        // A blank line before the prompt comes back.
        out.push_str("\n");
        if color {
            code_spans(&out.ansi().to_string())
        } else {
            out.to_string()
        }
    }
}

/// `text` with each `` `code` `` span on a line drawn as a literal, without
/// its backticks.
fn code_spans(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let mut rest = line;
        while let Some(open) = rest.find('`') {
            let Some(len) = rest[open + 1..].find('`') else {
                break;
            };
            let code = &rest[open + 1..open + 1 + len];
            let _ = write!(out, "{}{LITERAL}{code}{LITERAL:#}", &rest[..open]);
            rest = &rest[open + len + 2..];
        }
        out.push_str(rest);
    }
    out
}

/// A titled list of `(term, meaning)` pairs, the meanings lined up.
pub(super) fn section(title: &str, items: &[(&str, &str)]) -> StyledStr {
    let mut out = StyledStr::new();
    if items.is_empty() {
        return out;
    }
    let width = items
        .iter()
        .map(|(term, _)| term.chars().count())
        .max()
        .unwrap_or(0);
    paint(&mut out, HEADING, &format!("{title}:"));
    out.push_str("\n");
    for (term, meaning) in items {
        let pad = width - term.chars().count();
        out.push_str("  ");
        paint(&mut out, LITERAL, term);
        if meaning.is_empty() {
            out.push_str("\n");
        } else {
            let _ = writeln!(out, "{:pad$}  {meaning}", "");
        }
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
    let body = command.help_template("{all-args}").render_help();
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
        let color = color(shell);
        shell.out(&H::help().render(color))?;
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
            let color = color(shell);
            for (name, _, page) in matching {
                let page = page();
                let mut line = StyledStr::new();
                paint(&mut line, LITERAL, name);
                let line = if color {
                    line.ansi().to_string()
                } else {
                    line.to_string()
                };
                let text = if self.usage {
                    let usage = styled_usage(&page.usage);
                    let usage = if color {
                        usage.ansi().to_string()
                    } else {
                        usage.to_string()
                    };
                    format!("{line}: {usage}\n")
                } else if self.summary {
                    format!("{line} - {}\n", page.summary)
                } else {
                    page.render(color)
                };
                shell.out(&text)?;
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
    let mut out = StyledStr::new();
    paint(
        &mut out,
        Style::new().bold(),
        &format!("swagsh {} builtins", env!("CARGO_PKG_VERSION")),
    );
    out.push_str(". ");
    paint(&mut out, LITERAL, "help NAME");
    out.push_str(" shows one in full.\n\n");
    for (name, _, page) in BUILTINS {
        let page = page();
        out.push_str("  ");
        paint(&mut out, LITERAL, &format!("{name:<width$}"));
        if usage {
            let _ = writeln!(out, "  {}", styled_usage(&page.usage).ansi());
        } else {
            let _ = writeln!(out, "  {}", page.summary);
        }
    }
    out.push_str("\n");
    let text = if color(shell) {
        out.ansi().to_string()
    } else {
        out.to_string()
    };
    shell.out(&text)?;
    Ok(ExitStatus::SUCCESS)
}
