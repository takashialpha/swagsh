use std::fmt::Write as _;

use anyhow::{Result, bail};

use crate::eval::Shell;
use crate::expand::unescape;
use crate::jobs::ExitStatus;

#[allow(clippy::unnecessary_wraps)] // required by BuiltinFn signature
pub fn builtin_echo(_shell: &mut Shell, args: &[&str]) -> Result<ExitStatus> {
    let mut interpret_escapes = false;
    let mut no_newline = false;
    let mut start = 0;
    for arg in args {
        match *arg {
            "-n" => {
                no_newline = true;
                start += 1;
            }
            "-e" => {
                interpret_escapes = true;
                start += 1;
            }
            "-E" => {
                interpret_escapes = false;
                start += 1;
            }
            _ => break,
        }
    }
    let output = args[start..].join(" ");
    let output = if interpret_escapes {
        unescape(&output)
    } else {
        output
    };
    if no_newline {
        print!("{output}");
    } else {
        println!("{output}");
    }
    Ok(ExitStatus::SUCCESS)
}

pub fn builtin_printf(_shell: &mut Shell, args: &[&str]) -> Result<ExitStatus> {
    if args.is_empty() {
        bail!("printf: missing format string");
    }
    let fmt = unescape(args[0]);
    let mut arg_iter = args[1..].iter();
    let mut output = String::new();
    let mut chars = fmt.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            output.push(c);
            continue;
        }
        match chars.next() {
            Some('s') => output.push_str(arg_iter.next().copied().unwrap_or("")),
            Some('d') => {
                let n: i64 = arg_iter.next().copied().unwrap_or("0").parse().unwrap_or(0);
                output.push_str(&n.to_string());
            }
            Some('f') => {
                let f: f64 = arg_iter
                    .next()
                    .copied()
                    .unwrap_or("0")
                    .parse()
                    .unwrap_or(0.0);
                let _ = write!(output, "{f:.6}");
            }
            Some('%') | None => output.push('%'),
            Some(other) => {
                output.push('%');
                output.push(other);
            }
        }
    }
    print!("{output}");
    Ok(ExitStatus::SUCCESS)
}
