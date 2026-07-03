use anyhow::{Result, anyhow, bail};
use clap::Parser;
use rustix::process::{Pid, Signal, kill_process, kill_process_group};

use crate::errfmt::emit;
use crate::eval::Shell;
use crate::jobs::ExitStatus;

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

// Real-time signals (`SIGRTMIN`..`SIGRTMAX`) are listing-only here, not
// sendable: rustix's `Signal::from_raw_unchecked` docs explicitly say a
// `Signal` built from a number in that range "must not be used to send,
// consume, or block any signals" (a real syscall-safety constraint tied to
// glibc's own bookkeeping for the range, not rustix being overcautious);
// the safe constructor for that lives in the separate `rustix-libc-wrappers`
// crate, which isn't a dependency here. So `kill -l` can name and list
// them (pure display, no `Signal` value involved), but `kill -RTMIN+1 pid`
// errors out honestly instead of doing something unsound.
//
// The 34..=64 bounds are glibc's standard reservation (33 signals used by
// pthreads for cancellation/setuid internals, 31 left over for
// applications) on essentially every mainstream Linux distribution; a
// libc with a different split (musl, notably, doesn't reserve any) would
// see the wrong range here. Hard-coded rather than queried at runtime via
// libc's `SIGRTMIN()`/`SIGRTMAX()` functions, since that would mean adding
// `libc` as a direct dependency for two integers.
const SIGRTMIN: i32 = 34;
const SIGRTMAX: i32 = 64;

/// The conventional real-time signal naming: `SIGRTMIN`, `SIGRTMIN+1`, ...,
/// `SIGRTMAX-1`, `SIGRTMAX`, switching from a `RTMIN+` to a `RTMAX-` offset
/// at whichever is closer to `n` (ties go to `RTMIN+`: with `SIGRTMIN`/
/// `SIGRTMAX` 34/64, `49` is `RTMIN+15` not `RTMAX-15`).
fn rt_signal_name(n: i32) -> Option<String> {
    if !(SIGRTMIN..=SIGRTMAX).contains(&n) {
        return None;
    }
    let below = n - SIGRTMIN;
    let above = SIGRTMAX - n;
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
    if bare == "RTMIN" {
        return Some(SIGRTMIN);
    }
    if bare == "RTMAX" {
        return Some(SIGRTMAX);
    }
    if let Some(n) = bare.strip_prefix("RTMIN+") {
        let raw = SIGRTMIN + n.parse::<i32>().ok()?;
        return (raw <= SIGRTMAX).then_some(raw);
    }
    if let Some(n) = bare.strip_prefix("RTMAX-") {
        let raw = SIGRTMAX - n.parse::<i32>().ok()?;
        return (raw >= SIGRTMIN).then_some(raw);
    }
    None
}

/// Name lookup by raw signal number for `kill -l NUM`, covering both the
/// fixed table (1-31) and the dynamically-named real-time range. Pure
/// display: never constructs a `Signal`, so the real-time restriction
/// above doesn't apply to this direction.
fn signal_name_by_raw(n: i32) -> Option<String> {
    if let Some((name, _)) = SIGNAL_TABLE.iter().find(|(_, s)| s.as_raw() == n) {
        return Some((*name).to_owned());
    }
    rt_signal_name(n)
}

/// Number lookup by name for `kill -l NAME`, the other pure-display
/// direction `parse_signal` (which also has to reject the real-time case
/// for actually sending) doesn't cover.
fn signal_number_by_name(spec: &str) -> Option<i32> {
    let upper = spec.to_uppercase();
    let bare = upper.strip_prefix("SIG").unwrap_or(upper.as_str());
    if let Some((_, sig)) = SIGNAL_TABLE.iter().find(|(name, _)| *name == bare) {
        return Some(sig.as_raw());
    }
    parse_rt_name(bare)
}

fn parse_signal_name(s: &str) -> Option<Signal> {
    let upper = s.to_uppercase();
    let bare = upper.strip_prefix("SIG").unwrap_or(upper.as_str());
    SIGNAL_TABLE
        .iter()
        .find(|(name, _)| *name == bare)
        .map(|(_, sig)| *sig)
}

fn parse_signal(spec: &str) -> Result<Signal> {
    if let Ok(n) = spec.parse::<i32>() {
        if (SIGRTMIN..=SIGRTMAX).contains(&n) {
            bail!(
                "kill: real-time signals (SIGRTMIN..SIGRTMAX) can be listed with -l but not sent"
            );
        }
        return Signal::from_named_raw(n).ok_or_else(|| anyhow!("kill: invalid signal: {spec}"));
    }
    if let Some(sig) = parse_signal_name(spec) {
        return Ok(sig);
    }
    let upper = spec.to_uppercase();
    let bare = upper.strip_prefix("SIG").unwrap_or(upper.as_str());
    if parse_rt_name(bare).is_some() {
        bail!("kill: real-time signals (SIGRTMIN..SIGRTMAX) can be listed with -l but not sent");
    }
    Err(anyhow!("kill: invalid signal: {spec}"))
}

fn builtin_kill_list(args: &[&str]) -> Result<ExitStatus> {
    if args.is_empty() {
        for (i, (name, _)) in SIGNAL_TABLE.iter().enumerate() {
            println!("{:2}) SIG{name}", i + 1);
        }
        for n in SIGRTMIN..=SIGRTMAX {
            if let Some(name) = rt_signal_name(n) {
                println!("{n:2}) SIG{name}");
            }
        }
        return Ok(ExitStatus::SUCCESS);
    }
    for spec in args {
        if let Ok(n) = spec.parse::<i32>() {
            let name = signal_name_by_raw(n)
                .ok_or_else(|| anyhow!("kill: {spec}: invalid signal number"))?;
            println!("{name}");
        } else {
            let n = signal_number_by_name(spec)
                .ok_or_else(|| anyhow!("kill: invalid signal: {spec}"))?;
            println!("{n}");
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
            return builtin_kill_list(&specs);
        }

        let (sig, targets) = if let Some(spec) = self.signal_spec.or(self.signal_num) {
            (parse_signal(&spec)?, self.targets)
        } else if let Some(first) = self.targets.first()
            && first.starts_with('-')
            && first.len() > 1
        {
            (parse_signal(&first[1..])?, self.targets[1..].to_vec())
        } else {
            (Signal::TERM, self.targets)
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
                let pgid = find_job(shell, id)
                    .map(|j| j.pgid)
                    .ok_or_else(|| anyhow!("kill: no such job: {id}"))?;
                kill_process_group(pgid, sig)
                    .map_err(|e| anyhow!("kill: ({target}) - {}", crate::errfmt::strerror(e)))?;
            } else {
                let raw: i32 = target
                    .parse()
                    .map_err(|_| anyhow!("kill: invalid pid: {target}"))?;
                let pid =
                    Pid::from_raw(raw).ok_or_else(|| anyhow!("kill: invalid pid: {target}"))?;
                kill_process(pid, sig)
                    .map_err(|e| anyhow!("kill: ({target}) - {}", crate::errfmt::strerror(e)))?;
            }
        }
        Ok(ExitStatus::SUCCESS)
    }
}
