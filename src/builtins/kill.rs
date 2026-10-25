use anyhow::{Result, anyhow, bail};
use clap::Parser;
use rustix::process::Signal;

use crate::errfmt::emit;
use crate::eval::Shell;
use crate::jobs::ExitStatus;
use crate::sys::{kill, rt_signals};

use super::Builtin;
use super::jobs::find_job;

/// `(display name, signal)`, in the numeric order `kill -l` lists them.
/// rustix names a few of these differently from their C/POSIX spelling
/// (`ABORT` not `ABRT`, `ALARM` not `ALRM`, `CHILD` not `CHLD`, `VTALARM`
/// not `VTALRM`, `POWER` not `PWR`); the display names here are the
/// conventional `SIGxxx` ones `signal(7)` uses.
const SIGNAL_TABLE: &[(&str, Signal)] = &[
    ("HUP", Signal::HUP),
    ("INT", Signal::INT),
    ("QUIT", Signal::QUIT),
    ("ILL", Signal::ILL),
    ("TRAP", Signal::TRAP),
    ("ABRT", Signal::ABORT),
    ("BUS", Signal::BUS),
    ("FPE", Signal::FPE),
    ("KILL", Signal::KILL),
    ("USR1", Signal::USR1),
    ("SEGV", Signal::SEGV),
    ("USR2", Signal::USR2),
    ("PIPE", Signal::PIPE),
    ("ALRM", Signal::ALARM),
    ("TERM", Signal::TERM),
    ("STKFLT", Signal::STKFLT),
    ("CHLD", Signal::CHILD),
    ("CONT", Signal::CONT),
    ("STOP", Signal::STOP),
    ("TSTP", Signal::TSTP),
    ("TTIN", Signal::TTIN),
    ("TTOU", Signal::TTOU),
    ("URG", Signal::URG),
    ("XCPU", Signal::XCPU),
    ("XFSZ", Signal::XFSZ),
    ("VTALRM", Signal::VTALARM),
    ("PROF", Signal::PROF),
    ("WINCH", Signal::WINCH),
    ("IO", Signal::IO),
    ("PWR", Signal::POWER),
    ("SYS", Signal::SYS),
];

/// The conventional real-time signal naming: `SIGRTMIN`, `SIGRTMIN+1`, ...,
/// `SIGRTMAX-1`, `SIGRTMAX`, switching from a `RTMIN+` to a `RTMAX-` offset
/// at whichever is closer to `n` (ties go to `RTMIN+`: with `SIGRTMIN`/
/// `SIGRTMAX` 34/64, `49` is `RTMIN+15` not `RTMAX-15`).
fn rt_signal_name(n: i32) -> Option<String> {
    let rt = rt_signals();
    if !rt.contains(&n) {
        return None;
    }
    let below = n - rt.start();
    let above = rt.end() - n;
    Some(if below <= above {
        if below == 0 {
            "RTMIN".to_owned()
        } else {
            format!("RTMIN+{below}")
        }
    } else if above == 0 {
        "RTMAX".to_owned()
    } else {
        format!("RTMAX-{above}")
    })
}

/// The inverse of `rt_signal_name`: parses `RTMIN`, `RTMIN+N`, `RTMAX-N`,
/// or `RTMAX` (already `SIG`-stripped and upper-cased by the caller) back
/// to a raw signal number.
fn parse_rt_name(bare: &str) -> Option<i32> {
    let rt = rt_signals();
    let n = if bare == "RTMIN" {
        *rt.start()
    } else if bare == "RTMAX" {
        *rt.end()
    } else if let Some(n) = bare.strip_prefix("RTMIN+") {
        rt.start().checked_add(n.parse().ok()?)?
    } else {
        rt.end()
            .checked_sub(bare.strip_prefix("RTMAX-")?.parse().ok()?)?
    };
    rt.contains(&n).then_some(n)
}

/// The name of signal `n` without `SIG`, covering both the fixed table and
/// the real-time range.
pub(super) fn signal_name_by_raw(n: i32) -> Option<String> {
    if let Some((name, _)) = SIGNAL_TABLE.iter().find(|(_, s)| s.as_raw() == n) {
        return Some((*name).to_owned());
    }
    rt_signal_name(n)
}

/// The number of the signal named `spec`, in any case and with or without
/// `SIG`, real-time names included.
pub(super) fn signal_number_by_name(spec: &str) -> Option<i32> {
    let upper = spec.to_uppercase();
    let bare = upper.strip_prefix("SIG").unwrap_or(upper.as_str());
    if let Some((_, sig)) = SIGNAL_TABLE.iter().find(|(name, _)| *name == bare) {
        return Some(sig.as_raw());
    }
    parse_rt_name(bare)
}

/// Every signal number there is: the fixed table, then the real-time ones.
pub(super) fn all_signals() -> impl Iterator<Item = i32> {
    SIGNAL_TABLE
        .iter()
        .map(|(_, s)| s.as_raw())
        .chain(rt_signals())
}

/// A signal by name or number; `0` is the null signal, which only checks
/// that the target exists.
fn parse_signal(spec: &str) -> Result<i32> {
    let n = match spec.parse::<i32>() {
        Ok(n) => n,
        Err(_) => {
            signal_number_by_name(spec).ok_or_else(|| anyhow!("kill: invalid signal: {spec}"))?
        }
    };
    if n != 0 && signal_name_by_raw(n).is_none() {
        bail!("kill: invalid signal: {spec}");
    }
    Ok(n)
}

fn builtin_kill_list(shell: &mut Shell, args: &[&str]) -> Result<ExitStatus> {
    if args.is_empty() {
        for n in all_signals() {
            if let Some(name) = signal_name_by_raw(n) {
                shell.outln(&format!("{n:2}) SIG{name}"))?;
            }
        }
        return Ok(ExitStatus::SUCCESS);
    }
    for spec in args {
        if let Ok(n) = spec.parse::<i32>() {
            // An exit status of a process killed by a signal names it too.
            let name = signal_name_by_raw(if n > 128 { n - 128 } else { n })
                .ok_or_else(|| anyhow!("kill: {spec}: invalid signal number"))?;
            shell.outln(&name)?;
        } else {
            let n = signal_number_by_name(spec)
                .ok_or_else(|| anyhow!("kill: invalid signal: {spec}"))?;
            shell.outln(&n.to_string())?;
        }
    }
    Ok(ExitStatus::SUCCESS)
}

const KILL_USAGE: &str =
    "kill: usage: kill [-s sigspec | -n signum | -sigspec] pid | jobspec ... or kill -l [sigspec]";

/// `kill`'s flags proper (`-s`/`-n`/`-l`/`-L`) are ordinary clap options, but
/// the traditional `-SIGNAL`/`-9` shorthand isn't representable as a static
/// flag (its name varies with every signal): it's resolved from `targets`'s
/// first element in `run` below instead, treating anything after the known
/// options as "maybe a signal shorthand, then pids/jobspecs."
#[derive(Parser)]
#[command(
    name = "kill",
    about = "Send a signal to a process or job",
    trailing_var_arg = true
)]
pub struct KillBuiltin {
    /// Signal to send, by name or number
    #[arg(short = 's', conflicts_with = "signal_num")]
    signal_spec: Option<String>,
    /// Signal to send, by number
    #[arg(short = 'n', conflicts_with = "signal_spec")]
    signal_num: Option<String>,
    /// List signal names (or the name/number for each given spec)
    #[arg(short = 'l')]
    list: bool,
    /// Same as -l
    #[arg(short = 'L')]
    list_alt: bool,
    #[arg(allow_hyphen_values = true)]
    targets: Vec<String>,
}

impl Builtin for KillBuiltin {
    fn run(self, shell: &mut Shell) -> Result<ExitStatus> {
        if self.list || self.list_alt {
            let specs: Vec<&str> = self.targets.iter().map(String::as_str).collect();
            return builtin_kill_list(shell, &specs);
        }

        let (sig, targets) = if let Some(spec) = self.signal_spec.or(self.signal_num) {
            (parse_signal(&spec)?, self.targets)
        } else if let Some(first) = self.targets.first()
            && first.starts_with('-')
            && first.len() > 1
        {
            (parse_signal(&first[1..])?, self.targets[1..].to_vec())
        } else {
            (Signal::TERM.as_raw(), self.targets)
        };

        if targets.is_empty() {
            // A usage mistake, not a runtime failure: exit 2 like every
            // other usage error (clap's own included), not the plain 1
            // `?`-propagating this as a bare error would give it.
            emit(KILL_USAGE);
            return Ok(ExitStatus(2));
        }

        for target in &targets {
            if let Some(id_str) = target.strip_prefix('%') {
                let id: usize = id_str
                    .parse()
                    .map_err(|_| anyhow!("kill: invalid job: {target}"))?;
                let job = find_job(shell, id).ok_or_else(|| anyhow!("kill: no such job: {id}"))?;
                // Without job control the job has no process group of its
                // own, so signal each of its processes instead.
                let result = if shell.monitor {
                    kill(-job.pgid.as_raw_nonzero().get(), sig)
                } else {
                    job.pids
                        .iter()
                        .try_for_each(|&pid| kill(pid.as_raw_nonzero().get(), sig))
                };
                result.map_err(|e| anyhow!("kill: ({target}) - {}", crate::errfmt::strerror(e)))?;
            } else {
                let pid: i32 = target
                    .parse()
                    .map_err(|_| anyhow!("kill: invalid pid: {target}"))?;
                kill(pid, sig)
                    .map_err(|e| anyhow!("kill: ({target}) - {}", crate::errfmt::strerror(e)))?;
            }
        }
        Ok(ExitStatus::SUCCESS)
    }
}
