//! `test`/`[` can't go through clap: this isn't a command with flags, it's
//! a small boolean-expression grammar (`-a`/`-o`/`!`/`(...)` combining
//! unary file-test and binary comparison operators, arbitrary operand
//! counts depending on which operator is in play, `[` additionally
//! requiring a trailing `]`). clap parses "which flags were passed," not
//! "evaluate this expression tree": there's no flag surface here for it
//! to take over, the same way there's none for `printf`'s format-string
//! interpreter or `getopts`'s option-matching loop. This extends to
//! `--help` too: `test --help`/`[ --help ]` don't show help, they
//! evaluate `--help` as an ordinary (truthy, non-empty) string operand
//! and exit 0, the same as `test`/`[` with any other single non-empty
//! argument.

use std::os::unix::fs::{FileTypeExt, MetadataExt};

use anyhow::{Result, bail};
use rustix::fd::RawFd;
use rustix::fs::Access;

use crate::eval::Shell;
use crate::fd::fd_is_tty;
use crate::jobs::ExitStatus;

use super::help::{Example, HandParsed, Help, HelpPage};

/// The operators of `test`, for its help page.
const OPERATORS: &[(&str, &str)] = &[
    ("-e FILE", "FILE exists"),
    ("-f FILE", "FILE is a regular file"),
    ("-d FILE", "FILE is a directory"),
    ("-L FILE", "FILE is a symbolic link (also -h)"),
    ("-r/-w/-x FILE", "FILE is readable/writable/executable"),
    ("-s FILE", "FILE is not empty"),
    (
        "-b/-c/-p/-S FILE",
        "FILE is a block or character device, a FIFO, a socket",
    ),
    (
        "-g/-u/-k FILE",
        "FILE has its set-group-ID, set-user-ID, sticky bit",
    ),
    ("-O/-G FILE", "FILE is owned by you, by your group"),
    ("-N FILE", "FILE was modified since it was last read"),
    ("A -nt/-ot B", "File A is newer/older than B"),
    ("A -ef B", "A and B are the same file"),
    ("-t FD", "Descriptor FD is a terminal"),
    ("-z STRING", "STRING is empty"),
    ("-n STRING", "STRING is not empty (also: STRING alone)"),
    ("A = B", "The strings are equal (also ==)"),
    ("A != B", "The strings differ"),
    ("A < B", "A sorts before B (quote the <)"),
    (
        "A -eq B",
        "The numbers are equal; also -ne, -lt, -le, -gt, -ge",
    ),
    ("-v NAME", "Variable NAME is set"),
    ("! EXPR", "EXPR is false"),
    ("A -a B", "Both are true"),
    ("A -o B", "Either is true"),
    ("( EXPR )", "Grouping (quote the parentheses)"),
];

const TEST_EXAMPLES: &[Example] = &[
    Example {
        command: "test -f ~/.swagshrc && echo found",
        meaning: "Print found if the file exists",
    },
    Example {
        command: "[ \"$n\" -gt 0 ]",
        meaning: "Succeed if n is a positive number",
    },
];

const TEST_NOTES: &[&str] = &[
    "Quote expansions in tests: an empty unquoted $x vanishes.",
    "`[[ ]]` is the shell's own form, without word splitting.",
];

fn test_page(usage: &str) -> HelpPage {
    let mut page = HelpPage::manual(
        "Evaluate a conditional expression",
        usage,
        &[],
        TEST_EXAMPLES,
        TEST_NOTES,
    );
    page.body = super::help::section("Expressions", OPERATORS);
    page
}

pub struct Test;

impl Help for Test {
    fn help() -> HelpPage {
        test_page("test EXPRESSION")
    }
}

impl HandParsed for Test {
    const HELP_FLAG: bool = false;

    fn run(shell: &mut Shell, args: &[&str]) -> Result<ExitStatus> {
        Ok(eval_test(shell, args))
    }
}

pub struct Bracket;

impl Help for Bracket {
    fn help() -> HelpPage {
        test_page("[ EXPRESSION ]")
    }
}

impl HandParsed for Bracket {
    const HELP_FLAG: bool = false;

    fn run(shell: &mut Shell, args: &[&str]) -> Result<ExitStatus> {
        if args.last() != Some(&"]") {
            bail!("[: missing closing ]");
        }
        Ok(eval_test(shell, &args[..args.len() - 1]))
    }
}

fn eval_test(shell: &mut Shell, args: &[&str]) -> ExitStatus {
    if parse_or(shell, args).is_some_and(|(v, _)| v) {
        ExitStatus::SUCCESS
    } else {
        ExitStatus::FAILURE
    }
}

fn parse_or<'a>(shell: &mut Shell, args: &'a [&'a str]) -> Option<(bool, &'a [&'a str])> {
    let (mut val, mut rest) = parse_and(shell, args)?;
    while rest.first() == Some(&"-o") {
        let (rhs, r2) = parse_and(shell, &rest[1..])?;
        val = val || rhs;
        rest = r2;
    }
    Some((val, rest))
}

fn parse_and<'a>(shell: &mut Shell, args: &'a [&'a str]) -> Option<(bool, &'a [&'a str])> {
    let (mut val, mut rest) = parse_not(shell, args)?;
    while rest.first() == Some(&"-a") {
        let (rhs, r2) = parse_not(shell, &rest[1..])?;
        val = val && rhs;
        rest = r2;
    }
    Some((val, rest))
}

fn parse_not<'a>(shell: &mut Shell, args: &'a [&'a str]) -> Option<(bool, &'a [&'a str])> {
    if args.first() == Some(&"!") {
        let (val, rest) = parse_not(shell, &args[1..])?;
        return Some((!val, rest));
    }
    if args.first() == Some(&"(") {
        let close = args.iter().rposition(|&a| a == ")")?;
        let (val, _) = parse_or(shell, &args[1..close])?;
        return Some((val, &args[close + 1..]));
    }
    Some(parse_primary(shell, args))
}

fn parse_primary<'a>(shell: &mut Shell, args: &'a [&'a str]) -> (bool, &'a [&'a str]) {
    match args {
        [] => (false, &[]),
        [op, operand, rest @ ..] if let Some(val) = unary_test(shell, op, operand) => (val, rest),
        [a, "=" | "==", b, rest @ ..] => (a == b, rest),
        [a, "!=", b, rest @ ..] => (a != b, rest),
        [a, "<", b, rest @ ..] => (a < b, rest),
        [a, ">", b, rest @ ..] => (a > b, rest),
        [a, op, b, rest @ ..] if let Some(val) = file_compare(a, op, b) => (val, rest),
        [a, op, b, rest @ ..] if let Some(cmp) = integer_operator(op) => {
            let ai: i64 = a.trim().parse().unwrap_or(0);
            let bi: i64 = b.trim().parse().unwrap_or(0);
            (cmp(ai, bi), rest)
        }
        [s, rest @ ..] => (!s.is_empty(), rest),
    }
}

/// The comparison `-eq`, `-ne`, `-lt`, `-le`, `-gt` or `-ge` stands for.
pub fn integer_operator(op: &str) -> Option<fn(i64, i64) -> bool> {
    Some(match op {
        "-eq" => |a, b| a == b,
        "-ne" => |a, b| a != b,
        "-lt" => |a, b| a < b,
        "-le" => |a, b| a <= b,
        "-gt" => |a, b| a > b,
        "-ge" => |a, b| a >= b,
        _ => return None,
    })
}

/// A unary test, `-f path` and the like, if `op` names one.
pub fn unary_test(shell: &mut Shell, op: &str, operand: &str) -> Option<bool> {
    Some(match op {
        "-z" => operand.is_empty(),
        "-n" => !operand.is_empty(),
        "-t" => operand.parse::<RawFd>().is_ok_and(fd_is_tty),
        "-v" => shell.is_variable_set(operand),
        "-o" => super::env::option_is_on(shell, operand),
        _ => file_test(op, operand)?,
    })
}

/// A test on the file at `path`, if `op` names one.
fn file_test(op: &str, path: &str) -> Option<bool> {
    let p = std::path::Path::new(path);
    let meta = || std::fs::metadata(p);
    Some(match op {
        "-e" | "-a" => p.exists(),
        "-f" => p.is_file(),
        "-d" => p.is_dir(),
        "-L" | "-h" => p
            .symlink_metadata()
            .is_ok_and(|m| m.file_type().is_symlink()),
        "-r" => rustix::fs::access(p, Access::READ_OK).is_ok(),
        "-w" => rustix::fs::access(p, Access::WRITE_OK).is_ok(),
        "-x" => rustix::fs::access(p, Access::EXEC_OK).is_ok(),
        "-s" => meta().is_ok_and(|m| m.size() > 0),
        "-b" => meta().is_ok_and(|m| m.file_type().is_block_device()),
        "-c" => meta().is_ok_and(|m| m.file_type().is_char_device()),
        "-p" => meta().is_ok_and(|m| m.file_type().is_fifo()),
        "-S" => meta().is_ok_and(|m| m.file_type().is_socket()),
        "-u" => meta().is_ok_and(|m| m.mode() & 0o4000 != 0),
        "-g" => meta().is_ok_and(|m| m.mode() & 0o2000 != 0),
        "-k" => meta().is_ok_and(|m| m.mode() & 0o1000 != 0),
        "-O" => meta().is_ok_and(|m| m.uid() == rustix::process::geteuid().as_raw()),
        "-G" => meta().is_ok_and(|m| m.gid() == rustix::process::getegid().as_raw()),
        "-N" => meta().is_ok_and(|m| (m.mtime(), m.mtime_nsec()) > (m.atime(), m.atime_nsec())),
        _ => return None,
    })
}

/// `a -nt b`, `a -ot b` or `a -ef b`, if `op` is one of those.
pub fn file_compare(a: &str, op: &str, b: &str) -> Option<bool> {
    let modified = |p: &str| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    Some(match op {
        // A file that exists is newer than one that does not.
        "-nt" => match (modified(a), modified(b)) {
            (Some(x), Some(y)) => x > y,
            (x, y) => x.is_some() && y.is_none(),
        },
        "-ot" => match (modified(a), modified(b)) {
            (Some(x), Some(y)) => x < y,
            (x, y) => x.is_none() && y.is_some(),
        },
        "-ef" => match (std::fs::metadata(a), std::fs::metadata(b)) {
            (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
            _ => false,
        },
        _ => return None,
    })
}
