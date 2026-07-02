use anyhow::{Result, anyhow};
use clap::Parser;
use rustix::io::Errno;
use rustix::process::{Pid, Signal, WaitOptions, kill_process, kill_process_group, waitpgid};
use rustix::termios::tcsetpgrp;

use crate::eval::Shell;
use crate::jobs::{ExitStatus, Job, JobState, decode_wait_status};

use super::Builtin;

/// Parses the `%job` argument `fg`/`bg` take, defaulting to job 1 (`%%`,
/// the "current job", which this shell doesn't distinguish from job 1).
fn parse_job_id(args: &[&str]) -> usize {
    args.first()
        .and_then(|s| s.trim_start_matches('%').parse().ok())
        .unwrap_or(1)
}

fn find_job(shell: &Shell, id: usize) -> Option<&Job> {
    shell.jobs.iter().find(|j| j.id == id)
}

#[derive(Parser)]
#[command(name = "jobs", about = "List active jobs")]
pub struct JobsArgs {
    /// Also list each job's process group id
    #[arg(short = 'l')]
    long: bool,
    /// List only process ids
    #[arg(short = 'p')]
    pids_only: bool,
    /// List only running jobs
    #[arg(short = 'r')]
    running_only: bool,
    /// List only stopped jobs
    #[arg(short = 's')]
    stopped_only: bool,
}

impl Builtin for JobsArgs {
    fn run(self, shell: &mut Shell) -> Result<ExitStatus> {
        shell.jobs.reap_nonblocking();
        let entries: Vec<(usize, Pid, String, String)> = shell
            .jobs
            .iter()
            .filter(|j| {
                (!self.running_only || matches!(j.state, JobState::Running))
                    && (!self.stopped_only || matches!(j.state, JobState::Stopped))
            })
            .map(|j| {
                let state = match &j.state {
                    JobState::Running => "Running".into(),
                    JobState::Stopped => "Stopped".into(),
                    JobState::Done(s) => format!("Done({})", s.0),
                };
                (j.id, j.pgid, state, j.command.clone())
            })
            .collect();
        if !entries.is_empty() {
            shell.note_stdout("\n");
        }
        for (id, pgid, state, cmd) in entries {
            if self.pids_only {
                println!("{pgid}");
            } else if self.long {
                println!("[{id}]  {pgid}  {state}\t{cmd}");
            } else {
                println!("[{id}]  {state}\t{cmd}");
            }
        }
        Ok(ExitStatus::SUCCESS)
    }
}

pub fn builtin_fg(shell: &mut Shell, args: &[&str]) -> Result<ExitStatus> {
    let id = parse_job_id(args);
    let (pgid, cmd) = find_job(shell, id)
        .map(|j| (j.pgid, j.command.clone()))
        .ok_or_else(|| anyhow!("fg: no such job: {id}"))?;
    eprintln!("{cmd}");
    kill_process_group(pgid, Signal::CONT)?;
    let _ = tcsetpgrp(std::io::stdin(), pgid);
    let status = loop {
        match waitpgid(pgid, WaitOptions::UNTRACED) {
            Ok(Some((_, ws))) => {
                if let Some(exit) = decode_wait_status(ws) {
                    break exit;
                } else if ws.stopped() {
                    let _ = tcsetpgrp(std::io::stdin(), shell.pgid);
                    shell.restore_terminal();
                    break ExitStatus(130);
                }
            }
            Ok(None) => {}
            Err(e) if e == Errno::INTR => {}
            Err(e) => return Err(e.into()),
        }
    };
    let _ = tcsetpgrp(std::io::stdin(), shell.pgid);
    shell.restore_terminal();
    shell.jobs.remove(id);
    Ok(status)
}

pub fn builtin_bg(shell: &mut Shell, args: &[&str]) -> Result<ExitStatus> {
    let id = parse_job_id(args);
    let pgid = find_job(shell, id)
        .map(|j| j.pgid)
        .ok_or_else(|| anyhow!("bg: no such job: {id}"))?;
    kill_process_group(pgid, Signal::CONT)?;
    if let Some(job) = shell.jobs.by_pgid_mut(pgid) {
        job.state = JobState::Running;
    }
    eprintln!("[{id}]+ {pgid} &");
    Ok(ExitStatus::SUCCESS)
}

/// `(display name, signal)`, in the numeric order `kill -l` lists them.
/// rustix names a few of these differently from their C/POSIX spelling
/// (`ABORT` not `ABRT`, `ALARM` not `ALRM`, `CHILD` not `CHLD`, `VTALARM`
/// not `VTALRM`, `POWER` not `PWR`) — the display names here are the
/// conventional `SIGxxx` ones bash and `signal(7)` use.
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

fn parse_signal_name(s: &str) -> Option<Signal> {
    let upper = s.to_uppercase();
    let bare = upper.strip_prefix("SIG").unwrap_or(upper.as_str());
    SIGNAL_TABLE
        .iter()
        .find(|(name, _)| *name == bare)
        .map(|(_, sig)| *sig)
}

fn signal_name(sig: Signal) -> Option<&'static str> {
    SIGNAL_TABLE
        .iter()
        .find(|(_, s)| s.as_raw() == sig.as_raw())
        .map(|(name, _)| *name)
}

fn parse_signal(spec: &str) -> Result<Signal> {
    spec.parse::<i32>()
        .ok()
        .and_then(Signal::from_named_raw)
        .or_else(|| parse_signal_name(spec))
        .ok_or_else(|| anyhow!("kill: invalid signal: {spec}"))
}

fn builtin_kill_list(args: &[&str]) -> Result<ExitStatus> {
    if args.is_empty() {
        for (i, (name, _)) in SIGNAL_TABLE.iter().enumerate() {
            println!("{:2}) SIG{name}", i + 1);
        }
        return Ok(ExitStatus::SUCCESS);
    }
    for spec in args {
        if let Ok(n) = spec.parse::<i32>() {
            let sig = Signal::from_named_raw(n)
                .ok_or_else(|| anyhow!("kill: {spec}: invalid signal number"))?;
            println!("{}", signal_name(sig).unwrap_or("?"));
        } else {
            println!("{}", parse_signal(spec)?.as_raw());
        }
    }
    Ok(ExitStatus::SUCCESS)
}

pub fn builtin_kill(shell: &mut Shell, args: &[&str]) -> Result<ExitStatus> {
    if args.is_empty() {
        anyhow::bail!(
            "kill: usage: kill [-s sigspec | -n signum | -sigspec] pid | jobspec ... or kill -l [sigspec]"
        );
    }
    if args[0] == "-l" || args[0] == "-L" {
        return builtin_kill_list(&args[1..]);
    }
    let (sig, targets) = match args[0] {
        "-s" | "-n" => {
            let spec = args
                .get(1)
                .ok_or_else(|| anyhow!("kill: option requires an argument"))?;
            (parse_signal(spec)?, &args[2..])
        }
        arg if arg.starts_with('-') && arg.len() > 1 => (parse_signal(&arg[1..])?, &args[1..]),
        _ => (Signal::TERM, args),
    };
    for target in targets {
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
            let pid = rustix::process::Pid::from_raw(raw)
                .ok_or_else(|| anyhow!("kill: invalid pid: {target}"))?;
            kill_process(pid, sig)
                .map_err(|e| anyhow!("kill: ({target}) - {}", crate::errfmt::strerror(e)))?;
        }
    }
    Ok(ExitStatus::SUCCESS)
}
