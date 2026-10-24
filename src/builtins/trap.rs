//! `trap`: run a command when the shell receives a signal or exits.

use anyhow::{Result, anyhow, bail};
use rustix::process::Signal;

use super::kill::{parse_signal_name, signal_name_by_raw};
use crate::errfmt::emit;
use crate::eval::Shell;
use crate::expand::shell_quote_always;
use crate::jobs::ExitStatus;
use crate::signal::{sig_interrupt_action, trap_action};
use crate::sys::{SigHandler, sigaction};

/// `trap [-p] [-l] [ACTION CONDITION...]`.
///
/// Hand-parsed rather than through clap: POSIX gives the first operand two
/// meanings (an action, or with an unsigned integer first, a condition to
/// reset), and `-` as an action is an operand, not a flag.
///
/// # Errors
///
/// Returns an error for an unknown or untrappable condition.
pub fn builtin_trap(shell: &mut Shell, args: &[&str]) -> Result<ExitStatus> {
    let args = match args.first() {
        Some(&"--") => &args[1..],
        _ => args,
    };
    match args {
        [] | ["-p"] => return print_traps(shell, None),
        ["-p", conditions @ ..] => return print_traps(shell, Some(conditions)),
        ["-l"] => {
            for n in 1..32 {
                if let Some(name) = signal_name_by_raw(n) {
                    shell.outln(&format!("{n:2}) SIG{name}"))?;
                }
            }
            return Ok(ExitStatus::SUCCESS);
        }
        _ => {}
    }
    // A first operand that is an unsigned integer is itself a condition,
    // and every operand is reset.
    let (action, conditions) = if args[0].parse::<u32>().is_ok() {
        ("-", args)
    } else {
        (args[0], &args[1..])
    };
    if conditions.is_empty() {
        bail!("trap: usage: trap [-lp] [[ACTION] CONDITION ...]");
    }
    let mut status = ExitStatus::SUCCESS;
    for &condition in conditions {
        if let Err(e) = set_trap(shell, action, condition) {
            emit(e);
            status = ExitStatus::FAILURE;
        }
    }
    Ok(status)
}

fn set_trap(shell: &mut Shell, action: &str, condition: &str) -> Result<()> {
    let sig = parse_condition(condition)?;
    if sig == 0 {
        if action == "-" {
            shell.traps.remove(&0);
        } else {
            shell.traps.insert(0, action.to_owned());
        }
        return Ok(());
    }
    let signal = Signal::from_named_raw(sig)
        .ok_or_else(|| anyhow!("trap: {condition}: invalid signal specification"))?;
    if matches!(signal, Signal::KILL | Signal::STOP) {
        bail!("trap: {condition}: cannot be trapped");
    }
    let handler = match action {
        "-" => {
            shell.traps.remove(&sig);
            default_disposition(shell, signal)
        }
        "" => {
            shell.traps.insert(sig, String::new());
            SigHandler::Ignore
        }
        _ => {
            shell.traps.insert(sig, action.to_owned());
            trap_action()
        }
    };
    // SAFETY: the shell is single-threaded, and every handler installed here
    // only stores to an atomic.
    unsafe { sigaction(signal, handler) }.map_err(|e| anyhow!("trap: {condition}: {e}"))?;
    Ok(())
}

/// What the shell itself uses for `signal` when no trap is set: an
/// interactive shell catches `INT` and, like any job-control shell, ignores
/// the terminal stop signals.
fn default_disposition(shell: &Shell, signal: Signal) -> SigHandler {
    match signal {
        Signal::INT if shell.interactive => sig_interrupt_action(),
        Signal::TTOU | Signal::TTIN | Signal::TSTP if shell.interactive || shell.monitor => {
            SigHandler::Ignore
        }
        _ => SigHandler::Default,
    }
}

/// `EXIT`/`0`, a signal name with or without `SIG`, or a signal number.
fn parse_condition(condition: &str) -> Result<i32> {
    if condition.eq_ignore_ascii_case("EXIT") {
        return Ok(0);
    }
    if let Ok(n) = condition.parse::<i32>() {
        return Ok(n);
    }
    parse_signal_name(condition)
        .map(Signal::as_raw)
        .ok_or_else(|| anyhow!("trap: {condition}: invalid signal specification"))
}

/// Prints traps as `trap` commands that would set them again: all of them,
/// or just `conditions`.
fn print_traps(shell: &mut Shell, conditions: Option<&[&str]>) -> Result<ExitStatus> {
    let wanted = conditions
        .map(|c| {
            c.iter()
                .map(|c| parse_condition(c))
                .collect::<Result<Vec<_>>>()
        })
        .transpose()?;
    let traps: Vec<(i32, String)> = shell
        .traps
        .iter()
        .filter(|(sig, _)| wanted.as_ref().is_none_or(|w| w.contains(sig)))
        .map(|(&sig, action)| (sig, action.clone()))
        .collect();
    for (sig, action) in traps {
        let name = if sig == 0 {
            "EXIT".to_owned()
        } else {
            signal_name_by_raw(sig).unwrap_or_else(|| sig.to_string())
        };
        shell.outln(&format!("trap -- {} {name}", shell_quote_always(&action)))?;
    }
    Ok(ExitStatus::SUCCESS)
}
