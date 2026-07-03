use anyhow::{Result, bail};
use clap::Parser;

use crate::eval::Shell;
use crate::expand::unescape;
use crate::jobs::ExitStatus;

use super::Builtin;

/// A parsed `%[flags][width][.precision]conv` conversion.
struct Spec {
    left_align: bool,
    zero_pad: bool,
    plus_sign: bool,
    width: Option<usize>,
    precision: Option<usize>,
    conv: char,
}

/// Parses one `%...` conversion starting right after the `%`. Returns
/// `None` at end of input (a lone trailing `%`, printed literally).
fn parse_spec(chars: &mut std::iter::Peekable<std::str::Chars>) -> Option<Spec> {
    let mut left_align = false;
    let mut zero_pad = false;
    let mut plus_sign = false;
    loop {
        match chars.peek() {
            Some('-') => left_align = true,
            Some('0') => zero_pad = true,
            Some('+') => plus_sign = true,
            _ => break,
        }
        chars.next();
    }
    let mut width_digits = String::new();
    while chars.peek().is_some_and(char::is_ascii_digit) {
        width_digits.push(chars.next().unwrap());
    }
    let width = width_digits.parse().ok();
    let mut precision = None;
    if chars.peek() == Some(&'.') {
        chars.next();
        let mut prec_digits = String::new();
        while chars.peek().is_some_and(char::is_ascii_digit) {
            prec_digits.push(chars.next().unwrap());
        }
        precision = Some(prec_digits.parse().unwrap_or(0));
    }
    let conv = chars.next()?;
    Some(Spec {
        left_align,
        zero_pad,
        plus_sign,
        width,
        precision,
        conv,
    })
}

/// Pads `s` out to the spec's width: left-aligned with spaces, or
/// right-aligned with spaces/zeros. Zero-padding keeps a leading `-`/`+`
/// sign ahead of the padding rather than after it (`%05d` of `-3` is
/// `-0003`, not `000-3`).
fn apply_width(s: String, spec: &Spec) -> String {
    let Some(width) = spec.width else { return s };
    let len = s.chars().count();
    if len >= width {
        return s;
    }
    let pad = width - len;
    if spec.left_align {
        format!("{s}{}", " ".repeat(pad))
    } else if spec.zero_pad {
        if let Some(rest) = s.strip_prefix(['-', '+']) {
            format!("{}{}{rest}", &s[..1], "0".repeat(pad))
        } else {
            format!("{}{s}", "0".repeat(pad))
        }
    } else {
        format!("{}{s}", " ".repeat(pad))
    }
}

fn signed(n: i64, plus_sign: bool) -> String {
    if n >= 0 && plus_sign {
        format!("+{n}")
    } else {
        n.to_string()
    }
}

/// Renders one conversion, consuming its argument (if any) from `args`.
/// Missing arguments default conventionally: `""` for `%s`/`%c`, `0` for
/// every numeric conversion.
fn format_conv(spec: &Spec, args: &mut std::iter::Peekable<std::slice::Iter<&str>>) -> String {
    match spec.conv {
        '%' => "%".to_owned(),
        's' => {
            let mut s = args.next().copied().unwrap_or("").to_owned();
            if let Some(p) = spec.precision {
                s = s.chars().take(p).collect();
            }
            apply_width(s, spec)
        }
        'c' => {
            let s = args
                .next()
                .copied()
                .unwrap_or("")
                .chars()
                .next()
                .map(String::from)
                .unwrap_or_default();
            apply_width(s, spec)
        }
        'd' | 'i' => {
            let n: i64 = args
                .next()
                .copied()
                .unwrap_or("0")
                .trim()
                .parse()
                .unwrap_or(0);
            apply_width(signed(n, spec.plus_sign), spec)
        }
        'u' => {
            let n: i64 = args
                .next()
                .copied()
                .unwrap_or("0")
                .trim()
                .parse()
                .unwrap_or(0);
            apply_width((n as u64).to_string(), spec)
        }
        'x' => {
            let n: i64 = args
                .next()
                .copied()
                .unwrap_or("0")
                .trim()
                .parse()
                .unwrap_or(0);
            apply_width(format!("{:x}", n as u64), spec)
        }
        'X' => {
            let n: i64 = args
                .next()
                .copied()
                .unwrap_or("0")
                .trim()
                .parse()
                .unwrap_or(0);
            apply_width(format!("{:X}", n as u64), spec)
        }
        'o' => {
            let n: i64 = args
                .next()
                .copied()
                .unwrap_or("0")
                .trim()
                .parse()
                .unwrap_or(0);
            apply_width(format!("{:o}", n as u64), spec)
        }
        'f' => {
            let f: f64 = args
                .next()
                .copied()
                .unwrap_or("0")
                .trim()
                .parse()
                .unwrap_or(0.0);
            let precision = spec.precision.unwrap_or(6);
            let body = if f >= 0.0 && spec.plus_sign {
                format!("+{f:.precision$}")
            } else {
                format!("{f:.precision$}")
            };
            apply_width(body, spec)
        }
        // Unknown conversion: print it back literally rather than
        // silently eating an argument for a specifier we don't support.
        other => format!("%{other}"),
    }
}

/// Whether `fmt` contains any argument-consuming conversion (anything but
/// `%%`): a format with none never reuses arguments, no matter how many
/// are left over (`printf "hi\n" a b c` prints `hi` once, not three times).
fn has_arg_conversions(fmt: &str) -> bool {
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%'
            && let Some(spec) = parse_spec(&mut chars)
            && spec.conv != '%'
        {
            return true;
        }
    }
    false
}

fn format_once(fmt: &str, args: &mut std::iter::Peekable<std::slice::Iter<&str>>) -> String {
    let mut out = String::new();
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match parse_spec(&mut chars) {
            Some(spec) => out.push_str(&format_conv(&spec, args)),
            None => out.push('%'),
        }
    }
    out
}

// `printf`'s one real flag (`-v var`, assign the output to a shell
// variable instead of printing it) goes through clap like any other
// builtin's flags; FORMAT's own mini-language (`%s`, `%-10.3f`, `%b`, ...)
// is not an argument grammar at all, it's what `printf` computes, not how
// it was invoked, so that part (`unescape`/`format_once`/
// `has_arg_conversions` above) stays hand-written, the same as `test`'s
// expression evaluator or `getopts`'s option-matching state machine.
//
// FORMAT and ARGUMENTS share one trailing field rather than a separate
// `format: String` + `arguments: Vec<String>`, for the same reason `exec`
// and `command` do: split across two positional fields, clap resumes
// trying to recognize flags (`printf "-n"` would misparse as `-n` itself)
// for everything after the first one, even with `allow_hyphen_values` on
// both.
//
// (Plain `//`, not `///`: a multi-paragraph `///` doc comment here becomes
// clap's `long_about` even with an explicit `about =` set below, dumping
// this whole rationale into `printf --help` instead of just "Format and
// print data".)
#[derive(Parser)]
#[command(
    name = "printf",
    about = "Format and print data",
    trailing_var_arg = true
)]
pub struct PrintfBuiltin {
    /// Assign the output to shell variable VAR instead of printing it
    #[arg(short = 'v')]
    var: Option<String>,
    #[arg(allow_hyphen_values = true, value_name = "FORMAT")]
    format_and_args: Vec<String>,
}

impl Builtin for PrintfBuiltin {
    fn run(self, shell: &mut Shell) -> Result<ExitStatus> {
        let Some((fmt_raw, arguments)) = self.format_and_args.split_first() else {
            bail!("printf: missing format string");
        };
        let (fmt, _stopped_early) = unescape(fmt_raw);
        let cycles = if has_arg_conversions(&fmt) {
            // Reuse the format until every argument's been consumed,
            // spreading it across the whole argument list (e.g.
            // `printf "%s\n" a b c` prints three lines).
            arguments.len().max(1)
        } else {
            1
        };
        let arg_refs: Vec<&str> = arguments.iter().map(String::as_str).collect();
        let mut arg_iter = arg_refs.iter().peekable();
        let mut output = String::new();
        for _ in 0..cycles {
            output.push_str(&format_once(&fmt, &mut arg_iter));
            if arg_iter.peek().is_none() {
                break;
            }
        }
        match self.var {
            Some(name) => shell.env.set(&name, output),
            None => {
                print!("{output}");
                shell.note_stdout(&output);
            }
        }
        Ok(ExitStatus::SUCCESS)
    }
}
