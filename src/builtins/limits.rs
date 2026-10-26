//! `umask`, `ulimit` and `times`: the process attributes a shell has to
//! manage itself, since a child could only change its own.

use anyhow::{Result, anyhow, bail};
use clap::Parser;
use rustix::fs::Mode;
use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};

use super::Builtin;
use super::help::{Example, HandParsed, Help, HelpPage};
use crate::eval::Shell;
use crate::jobs::ExitStatus;
use crate::sys::cpu_times;

#[derive(Parser)]
#[command(name = "umask", about = "Show or set the file mode creation mask")]
pub struct UmaskBuiltin {
    /// Show the mask symbolically (`u=rwx,g=rx,o=rx`)
    #[arg(short = 'S')]
    symbolic: bool,
    /// An octal mask, or symbolic permissions to allow (`g-w`, `o=`)
    mask: Option<String>,
}

impl Builtin for UmaskBuiltin {
    fn run(self, shell: &mut Shell) -> Result<ExitStatus> {
        let current = current_umask();
        let Some(spec) = self.mask else {
            if self.symbolic {
                shell.outln(&symbolic_permissions(!current & 0o777))?;
            } else {
                shell.outln(&format!("{current:04o}"))?;
            }
            return Ok(ExitStatus::SUCCESS);
        };
        let mask = if spec.bytes().all(|b| (b'0'..=b'7').contains(&b)) {
            u32::from_str_radix(&spec, 8)
                .ok()
                .filter(|m| *m <= 0o777)
                .ok_or_else(|| anyhow!("umask: {spec}: octal number out of range"))?
        } else {
            !apply_symbolic(!current & 0o777, &spec)? & 0o777
        };
        rustix::process::umask(Mode::from_raw_mode(mask));
        Ok(ExitStatus::SUCCESS)
    }
}

fn current_umask() -> u32 {
    let mask = rustix::process::umask(Mode::empty());
    rustix::process::umask(mask);
    mask.as_raw_mode() & 0o777
}

/// `u=rwx,g=rx,o=rx` for the permission bits in `allowed`.
fn symbolic_permissions(allowed: u32) -> String {
    ["u", "g", "o"]
        .iter()
        .zip([6, 3, 0])
        .map(|(who, shift)| {
            let bits = (allowed >> shift) & 0o7;
            let perms: String = [(4, 'r'), (2, 'w'), (1, 'x')]
                .iter()
                .filter(|(bit, _)| bits & bit != 0)
                .map(|(_, c)| c)
                .collect();
            format!("{who}={perms}")
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// Applies `chmod`-style clauses (`u+w,go-rx,a=r`) to the allowed bits.
fn apply_symbolic(mut allowed: u32, spec: &str) -> Result<u32> {
    for clause in spec.split(',') {
        let who_len = clause.find(['+', '-', '=']).unwrap_or(clause.len());
        let (who, mut ops) = clause.split_at(who_len);
        let mut who_bits = 0;
        for c in who.chars() {
            who_bits |= match c {
                'u' => 0o700,
                'g' => 0o070,
                'o' => 0o007,
                'a' => 0o777,
                _ => bail!("umask: {spec}: invalid symbolic mode"),
            };
        }
        if who_bits == 0 {
            who_bits = 0o777;
        }
        if ops.is_empty() {
            bail!("umask: {spec}: invalid symbolic mode");
        }
        while let Some(op) = ops.chars().next() {
            let perms_len = ops[1..].find(['+', '-', '=']).map_or(ops.len(), |i| i + 1);
            let mut perm_bits = 0;
            for c in ops[1..perms_len].chars() {
                perm_bits |= match c {
                    'r' => 0o444,
                    'w' => 0o222,
                    'x' => 0o111,
                    _ => bail!("umask: {spec}: invalid symbolic mode"),
                };
            }
            let bits = perm_bits & who_bits;
            match op {
                '+' => allowed |= bits,
                '-' => allowed &= !bits,
                _ => allowed = (allowed & !who_bits) | bits,
            }
            ops = &ops[perms_len..];
        }
    }
    Ok(allowed)
}

/// One `ulimit` resource: its flag, `setrlimit(2)` resource, the unit its
/// value is shown in, and a description for `ulimit -a`.
struct Limit {
    flag: char,
    resource: Resource,
    unit: u64,
    what: &'static str,
}

const LIMITS: &[Limit] = &[
    Limit {
        flag: 'c',
        resource: Resource::Core,
        unit: 512,
        what: "core file size (blocks)",
    },
    Limit {
        flag: 'd',
        resource: Resource::Data,
        unit: 1024,
        what: "data seg size (kbytes)",
    },
    Limit {
        flag: 'e',
        resource: Resource::Nice,
        unit: 1,
        what: "scheduling priority",
    },
    Limit {
        flag: 'f',
        resource: Resource::Fsize,
        unit: 512,
        what: "file size (blocks)",
    },
    Limit {
        flag: 'i',
        resource: Resource::Sigpending,
        unit: 1,
        what: "pending signals",
    },
    Limit {
        flag: 'l',
        resource: Resource::Memlock,
        unit: 1024,
        what: "max locked memory (kbytes)",
    },
    Limit {
        flag: 'm',
        resource: Resource::Rss,
        unit: 1024,
        what: "max memory size (kbytes)",
    },
    Limit {
        flag: 'n',
        resource: Resource::Nofile,
        unit: 1,
        what: "open files",
    },
    Limit {
        flag: 'q',
        resource: Resource::Msgqueue,
        unit: 1,
        what: "POSIX message queues (bytes)",
    },
    Limit {
        flag: 'r',
        resource: Resource::Rtprio,
        unit: 1,
        what: "real-time priority",
    },
    Limit {
        flag: 's',
        resource: Resource::Stack,
        unit: 1024,
        what: "stack size (kbytes)",
    },
    Limit {
        flag: 't',
        resource: Resource::Cpu,
        unit: 1,
        what: "cpu time (seconds)",
    },
    Limit {
        flag: 'u',
        resource: Resource::Nproc,
        unit: 1,
        what: "max user processes",
    },
    Limit {
        flag: 'v',
        resource: Resource::As,
        unit: 1024,
        what: "virtual memory (kbytes)",
    },
    Limit {
        flag: 'x',
        resource: Resource::Locks,
        unit: 1,
        what: "file locks",
    },
];

/// `ulimit [-H|-S] [-a | -RESOURCE...] [LIMIT]`.
///
/// Hand-parsed: resource letters combine freely (`-Hn`), and which one a
/// LIMIT applies to is whichever came last, which clap cannot express.
///
pub struct Ulimit;

impl Help for Ulimit {
    fn help() -> HelpPage {
        let mut options = vec![
            ("-H".to_owned(), "Show or set the hard limit".to_owned()),
            (
                "-S".to_owned(),
                "Show or set the soft limit (both, when setting)".to_owned(),
            ),
            ("-a".to_owned(), "Show every limit".to_owned()),
        ];
        options.extend(
            LIMITS
                .iter()
                .map(|l| (format!("-{}", l.flag), l.what.to_owned())),
        );
        let options: Vec<(&str, &str)> = options
            .iter()
            .map(|(f, w)| (f.as_str(), w.as_str()))
            .collect();
        HelpPage::manual(
            "Show or set limits on the resources of the shell and its commands",
            "ulimit [-H|-S] [-a | -RESOURCE...] [LIMIT]",
            &options,
            &[
                Example {
                    command: "ulimit -n",
                    meaning: "Show how many files a command may open",
                },
                Example {
                    command: "ulimit -c unlimited",
                    meaning: "Allow core dumps of any size",
                },
            ],
            &["With no RESOURCE, -f (file size). LIMIT can be `unlimited`."],
        )
    }
}

impl HandParsed for Ulimit {
    fn run(shell: &mut Shell, args: &[&str]) -> Result<ExitStatus> {
        builtin_ulimit(shell, args)
    }
}

fn builtin_ulimit(shell: &mut Shell, args: &[&str]) -> Result<ExitStatus> {
    let (mut hard, mut soft, mut all) = (false, false, false);
    let mut chosen: Vec<&Limit> = Vec::new();
    let mut value = None;
    for &arg in args {
        match arg.strip_prefix('-') {
            Some(flags) if !flags.is_empty() && value.is_none() => {
                for c in flags.chars() {
                    match c {
                        'H' => hard = true,
                        'S' => soft = true,
                        'a' => all = true,
                        _ => chosen.push(
                            LIMITS
                                .iter()
                                .find(|l| l.flag == c)
                                .ok_or_else(|| anyhow!("ulimit: -{c}: invalid option"))?,
                        ),
                    }
                }
            }
            _ if value.is_none() => value = Some(arg),
            _ => bail!("ulimit: too many arguments"),
        }
    }
    if all {
        for limit in LIMITS {
            let shown = show(getrlimit(limit.resource), limit.unit, hard);
            shell.outln(&format!("{:<32}(-{}) {shown}", limit.what, limit.flag))?;
        }
        return Ok(ExitStatus::SUCCESS);
    }
    let limit = chosen.last().copied().unwrap_or(&LIMITS[3]);
    let Some(value) = value else {
        for limit in if chosen.is_empty() {
            vec![limit]
        } else {
            chosen
        } {
            shell.outln(&show(getrlimit(limit.resource), limit.unit, hard))?;
        }
        return Ok(ExitStatus::SUCCESS);
    };
    let new = if value == "unlimited" {
        None
    } else {
        let n: u64 = value
            .parse()
            .map_err(|_| anyhow!("ulimit: {value}: invalid number"))?;
        Some(
            n.checked_mul(limit.unit)
                .ok_or_else(|| anyhow!("ulimit: {value}: too large"))?,
        )
    };
    let mut rlimit = getrlimit(limit.resource);
    // Neither -H nor -S sets both, as in other shells.
    if hard || !soft {
        rlimit.maximum = new;
    }
    if soft || !hard {
        rlimit.current = new;
    }
    setrlimit(limit.resource, rlimit)
        .map_err(|e| anyhow!("ulimit: {}: {}", limit.what, crate::errfmt::strerror(e)))?;
    Ok(ExitStatus::SUCCESS)
}

fn show(rlimit: Rlimit, unit: u64, hard: bool) -> String {
    let value = if hard { rlimit.maximum } else { rlimit.current };
    value.map_or_else(|| "unlimited".to_owned(), |v| (v / unit).to_string())
}

#[derive(Parser)]
#[command(
    name = "times",
    about = "Show the CPU time used by the shell and its children"
)]
pub struct TimesBuiltin;

impl Builtin for TimesBuiltin {
    fn run(self, shell: &mut Shell) -> Result<ExitStatus> {
        for children in [false, true] {
            let t = cpu_times(children);
            shell.outln(&format!(
                "{} {}",
                minutes_seconds(t.user),
                minutes_seconds(t.system)
            ))?;
        }
        Ok(ExitStatus::SUCCESS)
    }
}

/// `1m2.345s`, the format `times` and `time` share.
#[must_use]
pub fn minutes_seconds(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    format!("{}m{}.{:03}s", secs / 60, secs % 60, d.subsec_millis())
}
