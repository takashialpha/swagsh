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
//! The password-database lookup behind `~user` ([`home_dir_of`]) and the
//! CPU-time query behind `times`/`time` ([`cpu_times`]) live here too, since
//! rustix has no API for either.
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
/// no builder here: every previous call site passed empty flags and an empty
/// mask too.
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
    let rc = unsafe { libc::sigaction(sig.as_raw(), &raw const act, core::ptr::null_mut()) };
    if rc < 0 {
        return Err(last_errno());
    }
    Ok(())
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
