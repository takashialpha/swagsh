//! The declaration commands: `declare` (and its synonym `typeset`),
//! `local`, `readonly` and `export`, which give variables attributes and
//! values, or list them; and `mapfile`/`readarray`.
//!
//! Hand-parsed rather than through clap: attributes are turned off with a
//! `+` prefix (`declare +x name`), which clap has no notion of, and an
//! assignment argument may stand for a whole array (see
//! [`Shell::declaration`]).

use std::fmt::Write as _;

use anyhow::{Result, anyhow, bail};
use clap::Parser;

use super::{Builtin, BuiltinFailed};
use crate::env::{Attrs, double_quote};
use crate::errfmt::emit;
use crate::eval::{ExpandedAssign, ExpandedValue, Shell};
use crate::expand::{is_name, shell_quote};
use crate::jobs::ExitStatus;

/// Where a declaration puts its variables.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    /// `declare`: local to the running function, if any, unless `-g`.
    Auto,
    /// `local`: local to the running function, which there must be.
    Local,
    /// `readonly`, `export`: global.
    Global,
}

/// The options of a declaration command.
#[derive(Default)]
#[allow(clippy::struct_excessive_bools)] // one per independent flag
struct Flags {
    add: Attrs,
    remove: Attrs,
    readonly: bool,
    /// `-x` (`Some(true)`) or `+x` (`Some(false)`).
    export: Option<bool>,
    print: bool,
    /// `-f`: functions, with their definitions.
    functions: bool,
    /// `-F`: functions, by name only.
    function_names: bool,
    global: bool,
}

impl Flags {
    /// Some attribute to give or take away was asked for.
    const fn has_attributes(&self) -> bool {
        !self.add.is_empty() || !self.remove.is_empty() || self.readonly || self.export.is_some()
    }
}

/// Splits the leading options off `args`, accepting only the letters in
/// `allowed`.
fn parse_flags<'a>(
    cmd: &str,
    args: &'a [&'a str],
    allowed: &str,
) -> Result<(Flags, &'a [&'a str])> {
    let mut flags = Flags::default();
    let mut i = 0;
    while let Some(&arg) = args.get(i) {
        if arg == "--" {
            i += 1;
            break;
        }
        let on = match arg.as_bytes() {
            [b'-', _, ..] => true,
            [b'+', _, ..] => false,
            _ => break,
        };
        for c in arg[1..].chars() {
            if !allowed.contains(c) {
                emit(format!(
                    "{cmd}: {}{c}: invalid option",
                    if on { '-' } else { '+' }
                ));
                emit(format!(
                    "{cmd}: usage: {cmd} [-{allowed}] [name[=value] ...]"
                ));
                return Err(BuiltinFailed(ExitStatus(2)).into());
            }
            let attr = match c {
                'a' => Attrs::INDEXED,
                'A' => Attrs::ASSOC,
                'i' => Attrs::INTEGER,
                'l' => Attrs::LOWER,
                'u' => Attrs::UPPER,
                'n' => Attrs::NAMEREF,
                'r' if on => {
                    flags.readonly = true;
                    continue;
                }
                'x' => {
                    flags.export = Some(on);
                    continue;
                }
                'p' => {
                    flags.print = true;
                    continue;
                }
                'f' => {
                    flags.functions = true;
                    continue;
                }
                'F' => {
                    flags.function_names = true;
                    continue;
                }
                'g' => {
                    flags.global = true;
                    continue;
                }
                _ => continue,
            };
            if on {
                flags.add = flags.add.union(attr);
            } else {
                flags.remove = flags.remove.union(attr);
            }
        }
        i += 1;
    }
    Ok((flags, &args[i..]))
}

/// `declare [-aAfFgilnprux] [name[=value] ...]`, and `typeset`.
///
/// # Errors
///
/// Returns an error for a bad option or a failed write.
pub fn builtin_declare(shell: &mut Shell, args: &[&str]) -> Result<ExitStatus> {
    let (flags, operands) = parse_flags("declare", args, "aAfFgilnprux")?;
    declare(shell, "declare", &flags, operands, Scope::Auto)
}

/// `local [-aAilnrux] [name[=value] ...]`: `declare` for the running
/// function only. A name given without a value keeps the value it had
/// outside until assigned.
///
/// # Errors
///
/// Returns an error outside a function, or for a bad option.
pub fn builtin_local(shell: &mut Shell, args: &[&str]) -> Result<ExitStatus> {
    let (flags, operands) = parse_flags("local", args, "aAilnrux")?;
    if !shell.env.in_function() {
        bail!("local: can only be used in a function");
    }
    declare(shell, "local", &flags, operands, Scope::Local)
}

/// `readonly [-aAp] [name[=value] ...]`.
///
/// # Errors
///
/// Returns an error for a bad option or a failed write.
pub fn builtin_readonly(shell: &mut Shell, args: &[&str]) -> Result<ExitStatus> {
    let (mut flags, operands) = parse_flags("readonly", args, "aAp")?;
    if operands.is_empty() {
        let lines: Vec<String> = shell
            .env
            .declared_names()
            .into_iter()
            .filter(|name| shell.env.is_readonly(name))
            .map(|name| posix_line(shell, "readonly", name))
            .collect();
        for line in lines {
            shell.outln(&line)?;
        }
        return Ok(ExitStatus::SUCCESS);
    }
    flags.readonly = true;
    declare(shell, "readonly", &flags, operands, Scope::Global)
}

/// `export [-np] [name[=value] ...]`. `-n` takes the export attribute away.
///
/// # Errors
///
/// Returns an error for a bad option or a failed write.
pub fn builtin_export(shell: &mut Shell, args: &[&str]) -> Result<ExitStatus> {
    let (mut flags, operands) = parse_flags("export", args, "np")?;
    let remove = args
        .iter()
        .take_while(|a| a.starts_with('-') && **a != "--")
        .any(|a| a.contains('n'));
    if operands.is_empty() && !remove {
        let lines: Vec<String> = shell
            .env
            .declared_names()
            .into_iter()
            .filter(|name| shell.env.is_exported(name))
            .map(|name| posix_line(shell, "export", name))
            .collect();
        for line in lines {
            shell.outln(&line)?;
        }
        return Ok(ExitStatus::SUCCESS);
    }
    flags.export = Some(!remove);
    flags.print = false;
    declare(shell, "export", &flags, operands, Scope::Global)
}

fn declare(
    shell: &mut Shell,
    cmd: &str,
    flags: &Flags,
    operands: &[&str],
    scope: Scope,
) -> Result<ExitStatus> {
    if flags.functions || flags.function_names {
        return print_functions(shell, cmd, flags.functions, operands);
    }
    if operands.is_empty() {
        return list(shell, flags);
    }
    let mut status = ExitStatus::SUCCESS;
    for &operand in operands {
        if flags.print {
            if let Some(line) = declaration_line(shell, operand) {
                shell.outln(&line)?;
            } else {
                emit(format!("{cmd}: {operand}: not found"));
                status = ExitStatus::FAILURE;
            }
            continue;
        }
        if let Err(e) = declare_one(shell, flags, operand, scope) {
            emit(format!("{cmd}: {e}"));
            status = ExitStatus::FAILURE;
        }
    }
    Ok(status)
}

/// Declares one operand: `name`, or an assignment.
fn declare_one(shell: &mut Shell, flags: &Flags, operand: &str, scope: Scope) -> Result<()> {
    let assign = shell.declaration(operand);
    let name = assign.as_ref().map_or_else(
        || operand.split('[').next().unwrap_or(operand).to_owned(),
        |a| a.name.clone(),
    );
    if !is_name(&name) {
        bail!("`{operand}': not a valid identifier");
    }
    if scope == Scope::Local || (scope == Scope::Auto && !flags.global) {
        shell.env.make_local(&name);
    }
    if flags.has_attributes() || assign.is_none() {
        shell.env.declare(&name, flags.add, flags.remove)?;
    }
    match &assign {
        // `declare -n ref=name` points the reference, wherever it pointed.
        Some(assign) if flags.add.contains(Attrs::NAMEREF) => {
            let target = match &assign.value {
                ExpandedValue::Scalar(v) => v.clone(),
                ExpandedValue::Array(_) => bail!("{name}: reference variable cannot be an array"),
            };
            shell.env.set_nameref(&name, &target);
        }
        Some(assign) => shell.apply_assign(assign)?,
        None => {}
    }
    if flags.readonly {
        shell.env.mark_readonly(&name);
    }
    match flags.export {
        Some(true) => shell.env.mark_exported(&name),
        Some(false) => shell.env.unexport(&name),
        None => {}
    }
    Ok(())
}

/// `declare` with no names: with `-p` or an attribute, every variable that
/// has the attributes, as `declare` commands; with nothing, every variable
/// as an assignment, then every function, as `set` lists them.
fn list(shell: &mut Shell, flags: &Flags) -> Result<ExitStatus> {
    let names: Vec<String> = shell
        .env
        .declared_names()
        .into_iter()
        .map(str::to_owned)
        .collect();
    if !flags.print && !flags.has_attributes() {
        for name in &names {
            if let Some(line) = assignment_text(shell, name) {
                shell.outln(&line)?;
            }
        }
        return print_functions(shell, "declare", true, &[]);
    }
    for name in &names {
        let attrs = shell.env.attrs(name).unwrap_or_default();
        let is_array = shell.env.array(name);
        let wanted = (!flags.add.contains(Attrs::INDEXED)
            || is_array.is_some_and(|a| !a.is_assoc()))
            && (!flags.add.contains(Attrs::ASSOC)
                || is_array.is_some_and(crate::env::Array::is_assoc))
            && [Attrs::INTEGER, Attrs::LOWER, Attrs::UPPER]
                .iter()
                .all(|&a| !flags.add.contains(a) || attrs.contains(a))
            && (!flags.readonly || shell.env.is_readonly(name))
            && (flags.export != Some(true) || shell.env.is_exported(name));
        if wanted && let Some(line) = declaration_line(shell, name) {
            shell.outln(&line)?;
        }
    }
    Ok(ExitStatus::SUCCESS)
}

/// The attribute letters `declare -p` shows for `name`, in bash's order.
#[must_use]
pub fn attribute_letters(shell: &Shell, name: &str) -> String {
    let attrs = shell.env.attrs(name).unwrap_or_default();
    let array = shell.env.array(name);
    let mut out = String::new();
    if array.is_some_and(|a| !a.is_assoc()) || (array.is_none() && attrs.contains(Attrs::INDEXED)) {
        out.push('a');
    }
    if array.is_some_and(crate::env::Array::is_assoc)
        || (array.is_none() && attrs.contains(Attrs::ASSOC))
    {
        out.push('A');
    }
    for (attr, c) in [
        (Attrs::INTEGER, 'i'),
        (Attrs::LOWER, 'l'),
        (Attrs::NAMEREF, 'n'),
    ] {
        if attrs.contains(attr) {
            out.push(c);
        }
    }
    if shell.env.is_readonly(name) {
        out.push('r');
    }
    if attrs.contains(Attrs::UPPER) {
        out.push('u');
    }
    if shell.env.is_exported(name) {
        out.push('x');
    }
    out
}

/// `declare -p name`'s line for `name`, if it is declared at all.
#[must_use]
pub fn declaration_line(shell: &Shell, name: &str) -> Option<String> {
    let set = shell.env.get_ref(name).is_some() || shell.env.array(name).is_some();
    if !set
        && shell.env.attrs(name).is_none()
        && !shell.env.is_readonly(name)
        && !shell.env.is_exported(name)
    {
        return None;
    }
    let letters = attribute_letters(shell, name);
    if shell
        .env
        .attrs(name)
        .is_some_and(|a| a.contains(Attrs::NAMEREF))
    {
        let target = shell.env.nameref_target(name).unwrap_or_default();
        return Some(format!(
            "declare -{letters} {name}={}",
            double_quote(&target)
        ));
    }
    let mut line = format!(
        "declare -{} {name}",
        if letters.is_empty() { "-" } else { &letters }
    );
    if let Some(array) = shell.env.array(name) {
        let _ = write!(line, "={}", array.to_source());
    } else if let Some(value) = shell.env.get_ref(name) {
        let _ = write!(line, "={}", double_quote(value));
    }
    Some(line)
}

/// `name` as an assignment that sets it again, as `set` lists variables.
pub(super) fn assignment_text(shell: &Shell, name: &str) -> Option<String> {
    if let Some(array) = shell.env.array(name) {
        return Some(format!("{name}={}", array.to_source()));
    }
    shell
        .env
        .get_ref(name)
        .map(|value| format!("{name}={}", shell_quote(value)))
}

/// A `readonly -p` or `export -p` line: the POSIX `cmd name=value` form for
/// a plain variable, a `declare` command for an array, which the POSIX form
/// cannot express.
fn posix_line(shell: &Shell, cmd: &str, name: &str) -> String {
    if shell.env.array(name).is_some() {
        return declaration_line(shell, name).unwrap_or_default();
    }
    shell.env.get_ref(name).map_or_else(
        || format!("{cmd} {name}"),
        |value| format!("{cmd} {name}={}", shell_quote(value)),
    )
}

/// `declare -f` (`definitions`) or `-F`: the named functions, or all.
fn print_functions(
    shell: &mut Shell,
    cmd: &str,
    definitions: bool,
    names: &[&str],
) -> Result<ExitStatus> {
    let all: Vec<String>;
    let names: Vec<&str> = if names.is_empty() {
        all = shell
            .env
            .function_names()
            .into_iter()
            .map(str::to_owned)
            .collect();
        all.iter().map(String::as_str).collect()
    } else {
        names.to_vec()
    };
    let mut status = ExitStatus::SUCCESS;
    for name in names {
        let Some(body) = shell.env.get_function(name) else {
            emit(format!("{cmd}: {name}: not found"));
            status = ExitStatus::FAILURE;
            continue;
        };
        if definitions {
            let source = crate::unparse::function(name, &body);
            shell.outln(&source)?;
        } else {
            shell.outln(&format!("declare -f {name}"))?;
        }
    }
    Ok(status)
}

/// `mapfile`/`readarray`: reads lines into an indexed array.
#[derive(Parser)]
#[command(
    name = "mapfile",
    about = "Read lines from standard input into an indexed array"
)]
pub struct MapfileBuiltin {
    /// End each line at the first character of DELIM instead of newline
    #[arg(short = 'd', value_name = "DELIM")]
    delim: Option<String>,
    /// Read at most COUNT lines (0: all)
    #[arg(short = 'n', value_name = "COUNT")]
    count: Option<usize>,
    /// Store from index ORIGIN instead of clearing the array first
    #[arg(short = 'O', value_name = "ORIGIN")]
    origin: Option<usize>,
    /// Skip the first COUNT lines
    #[arg(short = 's', value_name = "COUNT")]
    skip: Option<usize>,
    /// Remove the delimiter from the end of each line
    #[arg(short = 't')]
    trim: bool,
    /// Read from file descriptor FD instead of standard input
    #[arg(short = 'u', value_name = "FD")]
    fd: Option<i32>,
    /// The array to fill (default `MAPFILE`)
    array: Option<String>,
}

impl Builtin for MapfileBuiltin {
    fn run(self, shell: &mut Shell) -> Result<ExitStatus> {
        let name = self.array.unwrap_or_else(|| "MAPFILE".to_owned());
        if !is_name(&name) {
            bail!("mapfile: `{name}': not a valid identifier");
        }
        let delim = match self.delim.as_deref() {
            Some("") => 0,
            Some(d) => d.bytes().next().unwrap_or(b'\n'),
            None => b'\n',
        };
        let fd = self.fd.unwrap_or(0);
        let mut skip = self.skip.unwrap_or(0);
        let limit = self.count.filter(|&n| n > 0);
        let mut items = Vec::new();
        while limit.is_none_or(|n| items.len() < n) {
            let (mut line, found) = super::read::read_until_delim(fd, delim, true)
                .map_err(|e| anyhow!("mapfile: {}", crate::errfmt::strerror(e)))?;
            if !found && line.is_empty() {
                break;
            }
            if found && !self.trim {
                line.push(delim);
            }
            if skip > 0 {
                skip -= 1;
            } else {
                items.push(String::from_utf8_lossy(&line).into_owned());
            }
            if !found {
                break;
            }
        }
        let origin = self.origin;
        let items = items
            .into_iter()
            .enumerate()
            .map(|(i, v)| match origin {
                Some(o) => (Some((o + i).to_string()), false, v),
                None => (None, false, v),
            })
            .collect();
        let assign = ExpandedAssign {
            name,
            index: None,
            append: origin.is_some(),
            value: ExpandedValue::Array(items),
        };
        shell
            .apply_assign(&assign)
            .map_err(|e| anyhow!("mapfile: {e}"))?;
        Ok(ExitStatus::SUCCESS)
    }
}
