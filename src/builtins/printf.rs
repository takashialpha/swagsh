use anyhow::{Result, bail};
use clap::Parser;

use crate::errfmt::emit;
use crate::eval::Shell;
use crate::expand::{unescape, unescape_format};
use crate::jobs::ExitStatus;

use super::Builtin;

/// A width or precision: given in the format, or taken from the next
/// argument (`*`).
#[derive(Clone, Copy)]
enum Count {
    Fixed(usize),
    Arg,
}

/// A parsed `%[flags][width][.precision]conv` conversion.
#[derive(Default)]
struct Spec {
    /// The flags given, of `-0+ #`.
    flags: String,
    width: Option<Count>,
    precision: Option<Count>,
    conv: char,
    /// The `strftime` format of a `%(fmt)T` conversion.
    time_format: String,
}

impl Spec {
    fn has(&self, flag: char) -> bool {
        self.flags.contains(flag)
    }
}

/// A piece of a parsed format.
enum Piece {
    Text(String),
    Conv(Spec),
    /// A conversion character `printf` does not know.
    Invalid(char),
}

/// Splits `fmt` into its literal text and conversions.
fn parse_format(fmt: &str) -> Vec<Piece> {
    let mut pieces = Vec::new();
    let mut rest = fmt;
    loop {
        let (text, after) = rest
            .split_once('%')
            .map_or((rest, None), |(t, a)| (t, Some(a)));
        if !text.is_empty() {
            pieces.push(Piece::Text(unescape_format(text)));
        }
        let Some(after) = after else {
            return pieces;
        };
        let mut chars = after.chars().peekable();
        let piece = parse_spec(&mut chars);
        rest = &after[after.len() - chars.map(char::len_utf8).sum::<usize>()..];
        let Some(piece) = piece else {
            pieces.push(Piece::Text("%".to_owned()));
            return pieces;
        };
        pieces.push(piece);
    }
}

/// Parses one conversion, starting right after its `%`. `None` at the end
/// of the format (a lone trailing `%`, printed as it is).
fn parse_spec(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Option<Piece> {
    let mut spec = Spec::default();
    while let Some(flag) = chars.next_if(|c| "-0+ #".contains(*c)) {
        spec.flags.push(flag);
    }
    spec.width = parse_count(chars);
    if chars.next_if_eq(&'.').is_some() {
        spec.precision = Some(parse_count(chars).unwrap_or(Count::Fixed(0)));
    }
    while chars.next_if(|c| "hlLjzt".contains(*c)).is_some() {}
    spec.conv = chars.next()?;
    if spec.conv == '(' {
        let mut format = String::new();
        loop {
            match chars.next() {
                Some(')') => break,
                Some(c) => format.push(c),
                None => return Some(Piece::Invalid('(')),
            }
        }
        if chars.next_if_eq(&'T').is_none() {
            return Some(Piece::Invalid('('));
        }
        spec.conv = 'T';
        spec.time_format = format;
    }
    if !"%sbqcdiouxXeEfFgGaAT".contains(spec.conv) {
        return Some(Piece::Invalid(spec.conv));
    }
    Some(Piece::Conv(spec))
}

fn parse_count(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Option<Count> {
    if chars.next_if_eq(&'*').is_some() {
        return Some(Count::Arg);
    }
    let mut n: Option<usize> = None;
    while let Some(d) = chars.next_if(char::is_ascii_digit) {
        let digit = d.to_digit(10).map_or(0, |d| d as usize);
        n = Some(n.unwrap_or(0).saturating_mul(10).saturating_add(digit));
    }
    n.map(Count::Fixed)
}

/// The arguments, and whether one of them was not a valid number.
struct Args<'a> {
    rest: std::slice::Iter<'a, String>,
    failed: bool,
}

impl Args<'_> {
    fn next_str(&mut self) -> &str {
        self.rest.next().map_or("", String::as_str)
    }

    fn is_empty(&self) -> bool {
        self.rest.len() == 0
    }

    /// The next argument as an integer, as C reads one (`0x1f`, `017`), or
    /// the code of the character after a leading quote (`'a`).
    fn next_int(&mut self, unsigned: bool) -> i128 {
        let Some(arg) = self.rest.next() else {
            return 0;
        };
        let (value, valid) = parse_int(arg);
        let (min, max) = if unsigned {
            (-i128::from(u64::MAX), i128::from(u64::MAX))
        } else {
            (i128::from(i64::MIN), i128::from(i64::MAX))
        };
        if !valid {
            emit(format!("printf: {arg}: invalid number"));
            self.failed = true;
        } else if !(min..=max).contains(&value) {
            emit(format!("printf: {arg}: Numerical result out of range"));
            self.failed = true;
        }
        value.clamp(min, max)
    }

    fn next_float(&mut self) -> f64 {
        let Some(arg) = self.rest.next() else {
            return 0.0;
        };
        let (value, valid) = parse_float(arg);
        if !valid {
            emit(format!("printf: {arg}: invalid number"));
            self.failed = true;
        }
        value
    }

    /// The next argument as a `*` width or precision.
    fn next_count(&mut self) -> i64 {
        i64::try_from(self.next_int(false)).unwrap_or(0)
    }
}

/// `arg` read as an integer, and whether all of it was one. A partial
/// number counts what it has (`12abc` is 12).
fn parse_int(arg: &str) -> (i128, bool) {
    let s = arg.trim_start();
    if let Some(quoted) = s.strip_prefix(['\'', '"']) {
        return (
            quoted
                .chars()
                .next()
                .map_or(0, |c| i128::from(u32::from(c))),
            true,
        );
    }
    if s.is_empty() {
        return (0, arg.is_empty());
    }
    let negative = s.starts_with('-');
    let digits = s.strip_prefix(['-', '+']).unwrap_or(s);
    let (radix, digits) = if digits.starts_with("0x") || digits.starts_with("0X") {
        (16, &digits[2..])
    } else if digits.len() > 1 && digits.starts_with('0') {
        (8, &digits[1..])
    } else {
        (10, digits)
    };
    let mut value: i128 = 0;
    let mut used = 0;
    for c in digits.chars() {
        let Some(d) = c.to_digit(radix) else { break };
        value = value
            .saturating_mul(i128::from(radix))
            .saturating_add(i128::from(d));
        used += 1;
    }
    let valid = used > 0 && used == digits.len();
    (if negative { -value } else { value }, valid)
}

/// `arg` read as a floating point number, and whether all of it was one.
fn parse_float(arg: &str) -> (f64, bool) {
    let s = arg.trim_start();
    let unsigned = s.strip_prefix(['-', '+']).unwrap_or(s);
    if s.starts_with(['\'', '"']) || unsigned.starts_with("0x") || unsigned.starts_with("0X") {
        let (n, valid) = parse_int(arg);
        #[expect(
            clippy::cast_precision_loss,
            reason = "C's printf converts the same way"
        )]
        return (n as f64, valid);
    }
    if s.is_empty() {
        return (0.0, arg.is_empty());
    }
    if let Ok(value) = s.parse() {
        return (value, true);
    }
    let value = (1..s.len())
        .rev()
        .filter(|&end| s.is_char_boundary(end))
        .find_map(|end| s[..end].parse().ok())
        .unwrap_or(0.0);
    (value, false)
}

/// The resolved width, precision and alignment of a conversion.
#[derive(Clone, Copy)]
struct Layout {
    width: usize,
    precision: Option<usize>,
    left: bool,
}

/// `s` padded to `width` columns, on the right when `left`.
fn pad(s: &str, width: usize, left: bool) -> String {
    let len = s.chars().count();
    if len >= width {
        return s.to_owned();
    }
    let fill = " ".repeat(width - len);
    if left {
        format!("{s}{fill}")
    } else {
        format!("{fill}{s}")
    }
}

/// An integer conversion of `value`, laid out as C's `printf` does.
fn format_int(spec: &Spec, value: i128, layout: &Layout) -> String {
    let Layout {
        width,
        precision,
        left,
    } = *layout;
    let magnitude = if spec.conv == 'd' || spec.conv == 'i' {
        value.unsigned_abs()
    } else {
        // Negative numbers wrap around, as C converts them to unsigned.
        u128::from(i64::try_from(value).map_or_else(
            |_| u64::try_from(value).unwrap_or(u64::MAX),
            i64::cast_unsigned,
        ))
    };
    let mut digits = match spec.conv {
        'o' => format!("{magnitude:o}"),
        'x' => format!("{magnitude:x}"),
        'X' => format!("{magnitude:X}"),
        _ => magnitude.to_string(),
    };
    if precision == Some(0) && magnitude == 0 {
        digits.clear();
    }
    if let Some(p) = precision
        && digits.len() < p
    {
        digits.insert_str(0, &"0".repeat(p - digits.len()));
    }
    let prefix = match spec.conv {
        'd' | 'i' if value < 0 => "-",
        'd' | 'i' if spec.has('+') => "+",
        'd' | 'i' if spec.has(' ') => " ",
        'o' if spec.has('#') && !digits.starts_with('0') => "0",
        'x' if spec.has('#') && magnitude != 0 => "0x",
        'X' if spec.has('#') && magnitude != 0 => "0X",
        _ => "",
    };
    let len = prefix.len() + digits.len();
    if spec.has('0') && !left && precision.is_none() && len < width {
        return format!("{prefix}{}{digits}", "0".repeat(width - len));
    }
    pad(&format!("{prefix}{digits}"), width, left)
}

/// A floating point conversion of `value`, formatted by the C library.
fn format_float(spec: &Spec, value: f64, layout: &Layout) -> String {
    let mut c_spec = format!("%{}", spec.flags.replace('-', ""));
    if layout.left {
        c_spec.push('-');
    }
    if layout.width > 0 {
        c_spec.push_str(&layout.width.to_string());
    }
    if let Some(p) = layout.precision {
        c_spec.push('.');
        c_spec.push_str(&p.to_string());
    }
    c_spec.push(spec.conv);
    crate::sys::format_float(&c_spec, value).unwrap_or_default()
}

/// `s` quoted for the shell's input as bash's `%q` does, with backslashes.
fn shell_escape(s: &str) -> String {
    if s.is_empty() {
        return "''".to_owned();
    }
    if s.chars().any(char::is_control) {
        return crate::env::quote_for_input(s);
    }
    let mut out = String::with_capacity(s.len() + 4);
    for (i, c) in s.chars().enumerate() {
        if " !\"$&'()*,;<>?[\\]^`{|}".contains(c) || i == 0 && (c == '~' || c == '#') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Output of the format, run over the arguments it consumes.
struct Output {
    text: String,
    /// A `\c` in a `%b` argument stopped all output.
    stopped: bool,
}

impl Output {
    /// Writes the conversion `spec`, taking its arguments from `args`.
    fn convert(&mut self, shell: &Shell, spec: &Spec, args: &mut Args<'_>) {
        let mut left = spec.has('-');
        let width = match spec.width {
            Some(Count::Fixed(n)) => n,
            Some(Count::Arg) => {
                let n = args.next_count();
                left |= n < 0;
                usize::try_from(n.unsigned_abs()).unwrap_or(usize::MAX)
            }
            None => 0,
        };
        let precision = match spec.precision {
            Some(Count::Fixed(n)) => Some(n),
            Some(Count::Arg) => usize::try_from(args.next_count()).ok(),
            None => None,
        };
        let truncate = |s: String| match precision {
            Some(p) => s.chars().take(p).collect(),
            None => s,
        };
        let text = match spec.conv {
            '%' => "%".to_owned(),
            's' => truncate(args.next_str().to_owned()),
            'c' => args.next_str().chars().next().unwrap_or('\0').to_string(),
            'b' => {
                let (text, stopped) = unescape(args.next_str());
                self.stopped = stopped;
                truncate(text)
            }
            'q' => truncate(shell_escape(args.next_str())),
            'T' => {
                let time = if args.is_empty() {
                    -1
                } else {
                    args.next_int(false)
                };
                let time = match time {
                    -1 => crate::sys::epoch_seconds(),
                    -2 => shell.started_at,
                    t => i64::try_from(t).unwrap_or(0),
                };
                let format = if spec.time_format.is_empty() {
                    "%X"
                } else {
                    &spec.time_format
                };
                truncate(crate::sys::strftime_at(format, time))
            }
            'd' | 'i' | 'o' | 'u' | 'x' | 'X' => {
                let value = args.next_int(spec.conv != 'd' && spec.conv != 'i');
                let layout = Layout {
                    width,
                    precision,
                    left,
                };
                self.text.push_str(&format_int(spec, value, &layout));
                return;
            }
            _ => {
                let value = args.next_float();
                let layout = Layout {
                    width,
                    precision,
                    left,
                };
                self.text.push_str(&format_float(spec, value, &layout));
                return;
            }
        };
        self.text.push_str(&pad(&text, width, left));
    }
}

// `printf`'s one real flag (`-v var`, assign the output to a shell
// variable instead of printing it) goes through clap like any other
// builtin's flags; FORMAT's own mini-language (`%s`, `%-10.3f`, `%b`, ...)
// is not an argument grammar at all, it's what `printf` computes, not how
// it was invoked, so that part (`parse_format` and `Output` above) stays
// hand-written, the same as `test`'s expression evaluator or `getopts`'s
// option-matching state machine.
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
    /// The format (`%s`, `%d`, `%5.2f`, ...), then the arguments it formats
    #[arg(allow_hyphen_values = true, value_name = "FORMAT")]
    format_and_args: Vec<String>,
}

impl Builtin for PrintfBuiltin {
    fn run(self, shell: &mut Shell) -> Result<ExitStatus> {
        let Some((fmt, arguments)) = self.format_and_args.split_first() else {
            bail!("printf: missing format string");
        };
        let pieces = parse_format(fmt);
        // The format is reused while arguments are left, as long as it
        // consumes some (`printf "%s\n" a b c` prints three lines).
        let consumes = pieces.iter().any(|piece| {
            matches!(piece, Piece::Conv(spec) if spec.conv != '%'
                || matches!(spec.width, Some(Count::Arg))
                || matches!(spec.precision, Some(Count::Arg)))
        });
        let mut args = Args {
            rest: arguments.iter(),
            failed: false,
        };
        let mut out = Output {
            text: String::new(),
            stopped: false,
        };
        let mut invalid = None;
        'cycles: loop {
            for piece in &pieces {
                match piece {
                    Piece::Text(text) => out.text.push_str(text),
                    Piece::Conv(spec) => out.convert(shell, spec, &mut args),
                    Piece::Invalid(c) => {
                        invalid = Some(*c);
                        break 'cycles;
                    }
                }
                if out.stopped {
                    break 'cycles;
                }
            }
            if !consumes || args.is_empty() {
                break;
            }
        }
        if let Some(name) = self.var {
            shell
                .assign_target(&name, out.text)
                .map_err(|e| anyhow::anyhow!("printf: {e}"))?;
        } else {
            shell.out(&out.text)?;
        }
        if let Some(c) = invalid {
            emit(format!("printf: `{c}': invalid format character"));
            return Ok(ExitStatus(1));
        }
        Ok(if args.failed {
            ExitStatus(1)
        } else {
            ExitStatus::SUCCESS
        })
    }
}
