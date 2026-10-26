//! The libc-like primitives a shell needs and `rustix` will not hand out.
//!
//! `fork`, `execve`, `sigaction` and `sigprocmask` exist in rustix only inside
//! its `runtime` module, and as of rustix 1.1.5 that module is no longer
//! reachable: it was renamed to carry a random suffix which upstream states
//! "will rotate periodically", explicitly as a counter-measure against
//! outside crates using it. Following that name would mean a build break on
//! every rustix release, so these four call `libc` directly instead. `libc`
//! is already in the dependency graph (via `rustyline`), and its 0.2 surface
//! is the most stable FFI target in the ecosystem.
//!
//! So do the other C library services rustix has no API for: the
//! password-database lookup behind `~user` ([`home_dir_of`]), the CPU-time
//! query behind `times`/`time` ([`cpu_times`]), locale collation for sorting
//! globs ([`set_collation`], [`strcoll`]), the real-time signals
//! ([`sigaction_raw`], [`kill`]) along with signal descriptions, and the
//! regular expressions of `[[ =~ ]]` ([`regex_match`]).
//!
//! Everything else the shell needs (`fs`, `io`, `pipe`, `termios`, `process`,
//! `fd`) still comes from rustix's stable public API.
//!
//! **This module is the only place in the crate allowed to name `libc::`.**
//! Keeping the FFI in one file is what made the rustix break a one-file fix
//! rather than a hunt through five, and it is the only reason the next such
//! break will be cheap too. The rule cannot be expressed as a lint, since
//! Rust has no per-module capability system, so `just boundaries` greps for it
//! and CI runs that alongside clippy. Its companion is [`crate::fd`], which
//! owns every raw-descriptor conversion for the same reason; between them they
//! hold the whole unsafe syscall surface, and ordinary interpreter code calls
//! only their safe wrappers.

use core::ffi::c_int;
use std::ffi::CStr;

use rustix::io::Errno;
use rustix::process::{Pid, Signal};

/// Which side of a [`fork`] the caller came out on.
pub enum Fork {
    /// The parent, carrying the new child's pid.
    ParentOf(Pid),
    /// The freshly created child.
    Child,
}

/// Forks the calling process.
///
/// Unlike rustix's `kernel_fork`, this goes through libc's `fork`, so glibc's
/// `pthread_atfork` handlers run. The shell is single-threaded, so there are
/// none of its own to worry about, and every other shell forks this way.
///
/// # Safety
///
/// The child side may run only async-signal-safe code until it execs or
/// exits. Every call site calls [`crate::signal::restore_child_signals`]
/// first, which is itself bound by the same rule.
///
/// # Errors
///
/// Returns the `fork(2)` errno if the child could not be created.
pub unsafe fn fork() -> Result<Fork, Errno> {
    // SAFETY: `fork` takes no arguments, and the caller upholds the child-side
    // restriction documented above.
    match unsafe { libc::fork() } {
        -1 => Err(last_errno()),
        0 => Ok(Fork::Child),
        // `fork` hands the parent a strictly positive pid, and `from_raw`
        // rejects only zero and negatives, so this branch always succeeds.
        pid => Pid::from_raw(pid).map(Fork::ParentOf).ok_or(Errno::SRCH),
    }
}

/// Replaces the current process image.
///
/// Returns only on failure, carrying the errno, exactly as `execve(2)` does.
///
/// # Safety
///
/// `argv` and `envp` must each be a null-terminated array of pointers to
/// NUL-terminated strings, all of which stay live for the duration of the
/// call.
pub unsafe fn execve(path: &CStr, argv: *const *const u8, envp: *const *const u8) -> Errno {
    // SAFETY: `path` is a `&CStr` and so already NUL-terminated; the caller
    // guarantees `argv`/`envp` are null-terminated arrays of valid C strings.
    unsafe { libc::execve(path.as_ptr(), argv.cast(), envp.cast()) };
    last_errno()
}

/// A signal disposition, narrowed to the three this shell ever installs.
#[derive(Clone, Copy)]
pub enum SigHandler {
    /// `SIG_DFL`: whatever the kernel does by default for the signal.
    Default,
    /// `SIG_IGN`: discard the signal.
    Ignore,
    /// A handler function, which must itself be async-signal-safe.
    Handler(extern "C" fn(c_int)),
}

/// Installs `handler` as `sig`'s disposition, with no flags and an empty mask.
///
/// That fixed shape is all this shell has ever needed, which is why there is
/// no builder here.
///
/// # Safety
///
/// A [`SigHandler::Handler`] must be async-signal-safe. In a forked child
/// this has to run before any async-signal-unsafe code does.
///
/// # Errors
///
/// Returns the `sigaction(2)` errno, which for the fixed set of signals used
/// here means only that the kernel rejected the signal number.
pub unsafe fn sigaction(sig: Signal, handler: SigHandler) -> Result<(), Errno> {
    // SAFETY: the caller upholds the same contract.
    unsafe { sigaction_raw(sig.as_raw(), handler) }
}

/// [`sigaction`] by signal number, for the real-time signals rustix has no
/// `Signal` value for.
///
/// # Safety
///
/// As for [`sigaction`].
///
/// # Errors
///
/// Returns the `sigaction(2)` errno, `EINVAL` for a number the kernel does
/// not accept.
pub unsafe fn sigaction_raw(signum: c_int, handler: SigHandler) -> Result<(), Errno> {
    // SAFETY: `libc::sigaction` is a plain C struct of integers and pointers,
    // for which an all-zero bit pattern is a valid starting state; every field
    // this shell cares about is assigned below.
    let mut act: libc::sigaction = unsafe { core::mem::zeroed() };
    act.sa_sigaction = match handler {
        SigHandler::Default => libc::SIG_DFL,
        SigHandler::Ignore => libc::SIG_IGN,
        // `sighandler_t` is a pointer-sized integer holding the function
        // address, which is exactly what C's `sigaction` expects.
        SigHandler::Handler(f) => f as libc::sighandler_t,
    };
    act.sa_flags = 0;
    // SAFETY: `act.sa_mask` is a valid, zeroed `sigset_t`.
    unsafe { libc::sigemptyset(&raw mut act.sa_mask) };
    // SAFETY: `act` is fully initialised above, and a null `oldact` simply
    // declines to be told the previous disposition.
    let rc = unsafe { libc::sigaction(signum, &raw const act, core::ptr::null_mut()) };
    if rc < 0 {
        return Err(last_errno());
    }
    Ok(())
}

/// Whether signal `signum` is currently ignored (`SIG_IGN`).
#[must_use]
pub fn is_ignored(signum: c_int) -> bool {
    // SAFETY: as in `sigaction_raw`, all-zero is a valid `sigaction`.
    let mut old: libc::sigaction = unsafe { core::mem::zeroed() };
    // SAFETY: a null `act` only queries; `old` is valid for writes.
    let rc = unsafe { libc::sigaction(signum, core::ptr::null(), &raw mut old) };
    rc == 0 && old.sa_sigaction == libc::SIG_IGN
}

/// The real-time signal numbers, `SIGRTMIN` to `SIGRTMAX`. The C library
/// reserves the first few for itself, so the bounds are its to say.
#[must_use]
pub fn rt_signals() -> core::ops::RangeInclusive<c_int> {
    libc::SIGRTMIN()..=libc::SIGRTMAX()
}

/// Sends signal `signum` to `pid`, or to process group `-pid` when `pid`
/// is negative, as `kill(2)` does. rustix only sends signals it has a
/// `Signal` value for, which leaves out the real-time ones.
///
/// # Errors
///
/// Returns the `kill(2)` errno.
pub fn kill(pid: c_int, signum: c_int) -> Result<(), Errno> {
    // SAFETY: `kill` takes two integers and touches no memory.
    if unsafe { libc::kill(pid, signum) } < 0 {
        return Err(last_errno());
    }
    Ok(())
}

/// The C library's description of signal `signum` (`strsignal(3)`), such
/// as `Terminated`.
#[must_use]
pub fn signal_description(signum: c_int) -> String {
    // SAFETY: `strsignal` returns a pointer to a NUL-terminated string that
    // stays valid until the next call; it is copied out at once.
    let ptr = unsafe { libc::strsignal(signum) };
    if ptr.is_null() {
        return format!("Signal {signum}");
    }
    // SAFETY: checked non-null above; see the previous comment.
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}

/// Unblocks every signal for the calling thread.
///
/// # Safety
///
/// Meant for a forked child before it execs; see [`fork`].
///
/// # Errors
///
/// Returns the `sigprocmask(2)` errno.
pub unsafe fn unblock_all_signals() -> Result<(), Errno> {
    // SAFETY: `sigset_t` is a plain bitset, for which all-zero is valid; the
    // `sigemptyset` below is the portable way to put it in a defined state.
    let mut set: libc::sigset_t = unsafe { core::mem::zeroed() };
    // SAFETY: `set` is a valid `sigset_t`.
    unsafe { libc::sigemptyset(&raw mut set) };
    // SAFETY: `set` is initialised above, and a null `oldset` declines to be
    // told the previous mask.
    let rc = unsafe { libc::sigprocmask(libc::SIG_SETMASK, &raw const set, core::ptr::null_mut()) };
    if rc < 0 {
        return Err(last_errno());
    }
    Ok(())
}

/// The errno left behind by the most recent failing libc call.
fn last_errno() -> Errno {
    Errno::from_raw_os_error(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
}

/// The home directory of the user named `user`, from the password database
/// (`getpwnam_r(3)`, so NSS sources such as LDAP count too). Used for `~user`
/// tilde expansion; rustix has no password-database API.
#[must_use]
pub fn home_dir_of(user: &str) -> Option<String> {
    let name = std::ffi::CString::new(user).ok()?;
    let mut buf = vec![0u8; 1024];
    loop {
        // SAFETY: `passwd` is a plain C struct of integers and pointers, for
        // which an all-zero bit pattern is a valid starting state.
        let mut pwd: libc::passwd = unsafe { core::mem::zeroed() };
        let mut result: *mut libc::passwd = core::ptr::null_mut();
        // SAFETY: every pointer is valid for the duration of the call, and
        // `buf.len()` is the true size of `buf`.
        let rc = unsafe {
            libc::getpwnam_r(
                name.as_ptr(),
                &raw mut pwd,
                buf.as_mut_ptr().cast(),
                buf.len(),
                &raw mut result,
            )
        };
        if rc == libc::ERANGE && buf.len() < 1 << 20 {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if rc != 0 || result.is_null() || pwd.pw_dir.is_null() {
            return None;
        }
        // SAFETY: on success `pw_dir` points at a NUL-terminated string
        // inside `buf`, which is still alive here.
        let dir = unsafe { CStr::from_ptr(pwd.pw_dir) };
        return Some(dir.to_string_lossy().into_owned());
    }
}

/// User and system CPU time, from `getrusage(2)`.
#[derive(Clone, Copy, Default)]
pub struct CpuTimes {
    pub user: std::time::Duration,
    pub system: std::time::Duration,
}

/// CPU time used by this process, or with `children`, by every child it
/// has waited for. Used by `times` and `time`; rustix has no `getrusage`.
#[must_use]
pub fn cpu_times(children: bool) -> CpuTimes {
    let who = if children {
        libc::RUSAGE_CHILDREN
    } else {
        libc::RUSAGE_SELF
    };
    // SAFETY: `rusage` is a plain C struct of integers, for which an
    // all-zero bit pattern is a valid starting state.
    let mut usage: libc::rusage = unsafe { core::mem::zeroed() };
    // SAFETY: `usage` is valid for writes for the duration of the call.
    if unsafe { libc::getrusage(who, &raw mut usage) } != 0 {
        return CpuTimes::default();
    }
    let duration = |tv: libc::timeval| {
        std::time::Duration::new(
            u64::try_from(tv.tv_sec).unwrap_or(0),
            u32::try_from(tv.tv_usec).unwrap_or(0) * 1000,
        )
    };
    CpuTimes {
        user: duration(usage.ru_utime),
        system: duration(usage.ru_stime),
    }
}

/// Switches the C library's collation (`LC_COLLATE`) to `locale`. False if
/// the locale is not installed, leaving the collation as it was.
#[must_use]
pub fn set_collation(locale: &str) -> bool {
    let Ok(name) = std::ffi::CString::new(locale) else {
        return false;
    };
    // SAFETY: `name` is a valid C string for the duration of the call; the
    // shell is single-threaded whenever it sorts.
    !unsafe { libc::setlocale(libc::LC_COLLATE, name.as_ptr()) }.is_null()
}

/// Compares two strings in the collation set by [`set_collation`]
/// (`strcoll(3)`).
#[must_use]
pub fn strcoll(a: &CStr, b: &CStr) -> core::cmp::Ordering {
    // SAFETY: both are valid NUL-terminated strings.
    unsafe { libc::strcoll(a.as_ptr(), b.as_ptr()) }.cmp(&0)
}

/// The byte range of a regular expression's whole match, then of each of its
/// groups, `None` for one that took no part.
pub type RegexGroups = Vec<Option<(usize, usize)>>;

/// Matches `text` against the POSIX extended regular expression `pattern`
/// (`regcomp(3)`, `regexec(3)`), for `[[ text =~ pattern ]]`, ignoring
/// case with `nocase`. On a match,
/// the byte range of the whole match and of each group, `None` for a group
/// that took no part.
///
/// # Errors
///
/// Returns the C library's message for a pattern that does not compile.
pub fn regex_match(pattern: &str, text: &str, nocase: bool) -> Result<Option<RegexGroups>, String> {
    let groups = 1 + regex_groups(pattern);
    let pattern = std::ffi::CString::new(pattern).map_err(|e| e.to_string())?;
    let text = std::ffi::CString::new(text).map_err(|e| e.to_string())?;
    // SAFETY: `regex_t` is a plain C struct, for which all-zero is a valid
    // state before `regcomp` fills it in.
    let mut re: libc::regex_t = unsafe { core::mem::zeroed() };
    let flags = libc::REG_EXTENDED | if nocase { libc::REG_ICASE } else { 0 };
    // SAFETY: `re` is valid for writes and `pattern` is a C string.
    let rc = unsafe { libc::regcomp(&raw mut re, pattern.as_ptr(), flags) };
    if rc != 0 {
        let mut buf = [0u8; 256];
        // SAFETY: `re` was passed to `regcomp`, and `buf` is writable for
        // its length.
        unsafe { libc::regerror(rc, &raw const re, buf.as_mut_ptr().cast(), buf.len()) };
        // SAFETY: as above; `regfree` is fine after a failed `regcomp`.
        unsafe { libc::regfree(&raw mut re) };
        let msg = CStr::from_bytes_until_nul(&buf).map_or_else(
            |_| "invalid regular expression".to_owned(),
            |m| m.to_string_lossy().into_owned(),
        );
        return Err(msg);
    }
    let mut matches = vec![
        libc::regmatch_t {
            rm_so: -1,
            rm_eo: -1
        };
        groups
    ];
    // SAFETY: `re` is compiled, `text` is a C string, and `matches` has room
    // for `groups` entries.
    let rc = unsafe {
        libc::regexec(
            &raw const re,
            text.as_ptr(),
            groups,
            matches.as_mut_ptr(),
            0,
        )
    };
    // SAFETY: `re` was compiled by `regcomp` and is not used again.
    unsafe { libc::regfree(&raw mut re) };
    if rc != 0 {
        return Ok(None);
    }
    Ok(Some(
        matches
            .iter()
            .map(|m| {
                Some((
                    usize::try_from(m.rm_so).ok()?,
                    usize::try_from(m.rm_eo).ok()?,
                ))
            })
            .collect(),
    ))
}

/// The number of groups in extended regular expression `pattern`: its
/// `(`s outside bracket expressions and not escaped. `regex_t` keeps its
/// own count private.
fn regex_groups(pattern: &str) -> usize {
    let mut groups = 0;
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                chars.next();
            }
            '(' => groups += 1,
            '[' => {
                // A `]` right after `[` or `[^` is part of the expression.
                chars.next_if_eq(&'^');
                chars.next_if_eq(&']');
                for c in chars.by_ref() {
                    if c == ']' {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    groups
}

/// The local time now, formatted by `strftime(3)` with `format`, for the
/// prompt's `\d`, `\t` and `\D{...}`.
#[must_use]
pub fn strftime_now(format: &str) -> String {
    strftime_at(format, epoch_seconds())
}

/// The time now, in seconds since the epoch.
#[must_use]
pub fn epoch_seconds() -> i64 {
    // SAFETY: a null argument asks `time` only for its return value.
    unsafe { libc::time(core::ptr::null_mut()) }
}

/// Seconds-since-the-epoch `time` in local time, formatted by
/// `strftime(3)` with `format` (`printf '%(fmt)T'`).
#[must_use]
pub fn strftime_at(format: &str, time: i64) -> String {
    let Ok(format) = std::ffi::CString::new(format) else {
        return String::new();
    };
    // SAFETY: `tm` is a plain C struct, for which all-zero is valid.
    let mut tm: libc::tm = unsafe { core::mem::zeroed() };
    // SAFETY: both pointers are valid for the call.
    if unsafe { libc::localtime_r(&raw const time, &raw mut tm) }.is_null() {
        return String::new();
    }
    let mut buf = [0u8; 256];
    // SAFETY: `buf` is writable for its length, `format` is a C string and
    // `tm` was filled in above.
    let len = unsafe {
        libc::strftime(
            buf.as_mut_ptr().cast(),
            buf.len(),
            format.as_ptr(),
            &raw const tm,
        )
    };
    String::from_utf8_lossy(&buf[..len]).into_owned()
}

/// `value` formatted by C's `snprintf(3)` with `spec`, a single floating
/// point conversion such as `%-10.3e` (`printf`'s `%e`, `%g`, `%a` and
/// `%f`). `None` if `spec` is not one.
#[must_use]
pub fn format_float(spec: &str, value: f64) -> Option<String> {
    let conv = spec.chars().last()?;
    let valid = spec.starts_with('%')
        && "eEfFgGaA".contains(conv)
        && spec[1..spec.len() - 1]
            .chars()
            .all(|c| c.is_ascii_digit() || "-+ #0.".contains(c));
    if !valid {
        return None;
    }
    let spec = std::ffi::CString::new(spec).ok()?;
    let mut buf = vec![0u8; 512];
    loop {
        // SAFETY: `buf` is writable for its length, and `spec` is one
        // floating point conversion, checked above, taking the one double.
        let n = unsafe { libc::snprintf(buf.as_mut_ptr().cast(), buf.len(), spec.as_ptr(), value) };
        let n = usize::try_from(n).ok()?;
        if n < buf.len() {
            return Some(String::from_utf8_lossy(&buf[..n]).into_owned());
        }
        buf.resize(n + 1, 0);
    }
}
