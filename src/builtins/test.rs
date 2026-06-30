use std::os::unix::fs::FileTypeExt;

use anyhow::{Result, bail};
use rustix::fd::RawFd;
use rustix::fs::Access;
use rustix::termios::isatty;

use crate::eval::Shell;
use crate::jobs::ExitStatus;

pub fn builtin_bracket(_shell: &mut Shell, args: &[&str]) -> Result<ExitStatus> {
    if args.last() != Some(&"]") {
        bail!("[: missing closing ]");
    }
    Ok(eval_test(&args[..args.len() - 1]))
}

#[allow(clippy::unnecessary_wraps)] // required by BuiltinFn signature
pub fn builtin_test(_shell: &mut Shell, args: &[&str]) -> Result<ExitStatus> {
    Ok(eval_test(args))
}

fn eval_test(args: &[&str]) -> ExitStatus {
    if parse_or(args).is_some_and(|(v, _)| v) {
        ExitStatus::SUCCESS
    } else {
        ExitStatus::FAILURE
    }
}

fn parse_or<'a>(args: &'a [&'a str]) -> Option<(bool, &'a [&'a str])> {
    let (mut val, mut rest) = parse_and(args)?;
    while rest.first() == Some(&"-o") {
        let (rhs, r2) = parse_and(&rest[1..])?;
        val = val || rhs;
        rest = r2;
    }
    Some((val, rest))
}

fn parse_and<'a>(args: &'a [&'a str]) -> Option<(bool, &'a [&'a str])> {
    let (mut val, mut rest) = parse_not(args)?;
    while rest.first() == Some(&"-a") {
        let (rhs, r2) = parse_not(&rest[1..])?;
        val = val && rhs;
        rest = r2;
    }
    Some((val, rest))
}

fn parse_not<'a>(args: &'a [&'a str]) -> Option<(bool, &'a [&'a str])> {
    if args.first() == Some(&"!") {
        let (val, rest) = parse_not(&args[1..])?;
        return Some((!val, rest));
    }
    if args.first() == Some(&"(") {
        let close = args.iter().rposition(|&a| a == ")")?;
        let (val, _) = parse_or(&args[1..close])?;
        return Some((val, &args[close + 1..]));
    }
    Some(parse_primary(args))
}

fn parse_primary<'a>(args: &'a [&'a str]) -> (bool, &'a [&'a str]) {
    match args {
        [] => (false, &[]),

        [op, path, rest @ ..]
            if matches!(
                *op,
                "-e" | "-f"
                    | "-d"
                    | "-r"
                    | "-w"
                    | "-x"
                    | "-s"
                    | "-L"
                    | "-h"
                    | "-b"
                    | "-c"
                    | "-p"
                    | "-S"
                    | "-u"
                    | "-g"
                    | "-k"
            ) =>
        {
            use std::fs as sfs;
            use std::os::unix::fs::MetadataExt;
            let p = std::path::Path::new(path);
            let val = match *op {
                "-e" => p.exists(),
                "-f" => p.is_file(),
                "-d" => p.is_dir(),
                "-L" | "-h" => p
                    .symlink_metadata()
                    .is_ok_and(|m| m.file_type().is_symlink()),
                "-r" => rustix::fs::access(p, Access::READ_OK).is_ok(),
                "-w" => rustix::fs::access(p, Access::WRITE_OK).is_ok(),
                "-x" => rustix::fs::access(p, Access::EXEC_OK).is_ok(),
                "-s" => sfs::metadata(p).is_ok_and(|m| m.size() > 0),
                "-b" => sfs::metadata(p).is_ok_and(|m| m.file_type().is_block_device()),
                "-c" => sfs::metadata(p).is_ok_and(|m| m.file_type().is_char_device()),
                "-p" => sfs::metadata(p).is_ok_and(|m| m.file_type().is_fifo()),
                "-S" => sfs::metadata(p).is_ok_and(|m| m.file_type().is_socket()),
                "-u" => sfs::metadata(p).is_ok_and(|m| m.mode() & 0o4000 != 0),
                "-g" => sfs::metadata(p).is_ok_and(|m| m.mode() & 0o2000 != 0),
                "-k" => sfs::metadata(p).is_ok_and(|m| m.mode() & 0o1000 != 0),
                _ => false,
            };
            (val, rest)
        }

        ["-z", s, rest @ ..] => (s.is_empty(), rest),
        ["-n", s, rest @ ..] => (!s.is_empty(), rest),

        ["-t", fd_str, rest @ ..] => {
            let val = fd_str.parse::<RawFd>().is_ok_and(|fd| {
                // SAFETY: we only borrow the fd for the isatty call; not closed.
                isatty(unsafe { rustix::fd::BorrowedFd::borrow_raw(fd) })
            });
            (val, rest)
        }

        [a, "=" | "==", b, rest @ ..] => (a == b, rest),
        [a, "!=", b, rest @ ..] => (a != b, rest),
        [a, "<", b, rest @ ..] => (a < b, rest),
        [a, ">", b, rest @ ..] => (a > b, rest),

        [a, op, b, rest @ ..] if matches!(*op, "-eq" | "-ne" | "-lt" | "-le" | "-gt" | "-ge") => {
            let ai: i64 = a.parse().unwrap_or(0);
            let bi: i64 = b.parse().unwrap_or(0);
            let val = match *op {
                "-eq" => ai == bi,
                "-ne" => ai != bi,
                "-lt" => ai < bi,
                "-le" => ai <= bi,
                "-gt" => ai > bi,
                "-ge" => ai >= bi,
                _ => false,
            };
            (val, rest)
        }

        [s, rest @ ..] => (!s.is_empty(), rest),
    }
}
