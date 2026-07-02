//! `clap`-based flag parsing for [`super::Builtin`] implementations.
//!
//! Builtins take POSIX-style single-dash flags (`cd -L`, `read -r -p`, ...),
//! not a full CLI-app argument model, so [`parse_args`] strips `clap`'s
//! defaults down to that shape: no `--version`, argv[0] omitted (builtins
//! are already matched by name before their handler runs), and errors
//! reformatted to a single line on the builtin's own name instead of
//! clap's multi-paragraph "Usage: ..." block. Color is untouched (`clap`
//! already defaults to `ColorChoice::Auto`, matching the rest of swagsh's
//! output: real color on a terminal, plain text piped/NO_COLOR).
use anyhow::Result;
use clap::{Command, Parser, error::ErrorKind};

use super::Builtin;
use crate::errfmt::emit;
use crate::eval::Shell;
use crate::jobs::ExitStatus;

/// Outcome of parsing a builtin's arguments: the parsed struct, `-h`
/// already having printed help, or a bad-flag usage error already printed.
/// Kept distinct from a builtin's own runtime errors (a bad directory, a
/// missing file, ...) so [`dispatch`] can give usage mistakes the
/// conventional exit status 2 while runtime failures still get 1, the same
/// distinction most Unix tools make.
enum ParsedArgs<T> {
    Ok(T),
    Help,
    UsageError,
}

/// Adapts any [`Builtin`] into the plain
/// `fn(&mut Shell, &[&str]) -> Result<ExitStatus>` shape the `BUILTINS`
/// lookup table stores. `dispatch::<CdArgs>` slots directly into that
/// table in place of a hand-written wrapper function.
pub fn dispatch<B: Builtin>(shell: &mut Shell, args: &[&str]) -> Result<ExitStatus> {
    match parse_args::<B>(args)? {
        ParsedArgs::Ok(b) => b.run(shell),
        ParsedArgs::Help => Ok(ExitStatus::SUCCESS),
        ParsedArgs::UsageError => Ok(ExitStatus(2)),
    }
}

fn command_for<T: Parser>() -> Command {
    T::command().no_binary_name(true).disable_version_flag(true)
}

/// Parses `args` as `T`, using `T`'s own command name for error text.
fn parse_args<T: Parser>(args: &[&str]) -> Result<ParsedArgs<T>> {
    let cmd = command_for::<T>();
    let name = cmd.get_name().to_owned();
    match cmd.try_get_matches_from(args) {
        Ok(matches) => Ok(ParsedArgs::Ok(T::from_arg_matches(&matches)?)),
        Err(e)
            if matches!(
                e.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
            ) =>
        {
            // `Error::print` (not `Display`) is what actually applies the
            // configured `ColorChoice` against the real output stream.
            let _ = e.print();
            Ok(ParsedArgs::Help)
        }
        Err(e) => {
            let rendered = e.render().to_string();
            let first_line = rendered
                .lines()
                .next()
                .unwrap_or(&rendered)
                .trim_start_matches("error: ");
            emit(format!("{name}: {first_line}"));
            Ok(ParsedArgs::UsageError)
        }
    }
}
