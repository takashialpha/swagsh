use anyhow::{Result, anyhow};
use rustix::io::Errno;
use rustix::process::{Signal, WaitOptions, kill_process, kill_process_group, waitpgid};
use rustix::termios::tcsetpgrp;

use crate::eval::Shell;
use crate::jobs::{ExitStatus, Job, JobState, decode_wait_status};

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

#[allow(clippy::unnecessary_wraps)] // required by BuiltinFn signature
pub fn builtin_jobs(shell: &mut Shell, _args: &[&str]) -> Result<ExitStatus> {
    shell.jobs.reap_nonblocking();
    let entries: Vec<(usize, String, String)> = shell
        .jobs
        .iter()
        .map(|j| {
            let state = match &j.state {
                JobState::Running => "Running".into(),
                JobState::Stopped => "Stopped".into(),
                JobState::Done(s) => format!("Done({})", s.0),
            };
            (j.id, state, j.command.clone())
        })
        .collect();
    for (id, state, cmd) in entries {
        println!("[{id}]  {state}\t{cmd}");
    }
    Ok(ExitStatus::SUCCESS)
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
                    break ExitStatus(130);
                }
            }
            Ok(None) => {}
            Err(e) if e == Errno::INTR => {}
            Err(e) => return Err(e.into()),
        }
    };
    let _ = tcsetpgrp(std::io::stdin(), shell.pgid);
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

pub fn builtin_kill(shell: &mut Shell, args: &[&str]) -> Result<ExitStatus> {
    if args.is_empty() {
        anyhow::bail!("kill: usage: kill [-signal] pid|%job");
    }
    let (sig, targets) = if args[0].starts_with('-') {
        let sig_str = args[0].trim_start_matches('-');
        let sig = sig_str
            .parse::<i32>()
            .ok()
            .and_then(Signal::from_named_raw)
            .or_else(|| parse_signal_name(sig_str))
            .ok_or_else(|| anyhow!("kill: invalid signal: {sig_str}"))?;
        (sig, &args[1..])
    } else {
        (Signal::TERM, args)
    };
    for target in targets {
        if let Some(id_str) = target.strip_prefix('%') {
            let id: usize = id_str
                .parse()
                .map_err(|_| anyhow!("kill: invalid job: {target}"))?;
            let pgid = find_job(shell, id)
                .map(|j| j.pgid)
                .ok_or_else(|| anyhow!("kill: no such job: {id}"))?;
            kill_process_group(pgid, sig)?;
        } else {
            let raw: i32 = target
                .parse()
                .map_err(|_| anyhow!("kill: invalid pid: {target}"))?;
            let pid = rustix::process::Pid::from_raw(raw)
                .ok_or_else(|| anyhow!("kill: invalid pid: {target}"))?;
            kill_process(pid, sig)?;
        }
    }
    Ok(ExitStatus::SUCCESS)
}

fn parse_signal_name(s: &str) -> Option<Signal> {
    let upper = s.to_uppercase();
    let bare = upper.strip_prefix("SIG").unwrap_or(upper.as_str());
    match bare {
        "HUP" => Some(Signal::HUP),
        "INT" => Some(Signal::INT),
        "QUIT" => Some(Signal::QUIT),
        "KILL" => Some(Signal::KILL),
        "TERM" => Some(Signal::TERM),
        "STOP" => Some(Signal::STOP),
        "CONT" => Some(Signal::CONT),
        "TSTP" => Some(Signal::TSTP),
        "USR1" => Some(Signal::USR1),
        "USR2" => Some(Signal::USR2),
        _ => None,
    }
}
