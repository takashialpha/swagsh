use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};

use rustix::fd::RawFd;

use rustix::process::Signal;

use crate::sys::{SigHandler, sigaction, unblock_all_signals};

/// Set by [`handle_sigint`] when the interactive shell's own `SIGINT`
/// handler fires; drained by [`take_interrupted`]. An atomic store is the
/// only thing that's safe to do from inside a signal handler.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_sigint(_signum: core::ffi::c_int) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

/// The interactive shell's own disposition for `SIGINT`.
///
/// Installed once at startup (see `Shell::new`) so a `^C` typed while a
/// builtin loop or function is running (i.e. no forked child to absorb the
/// default terminate action) sets a flag the interpreter can notice instead
/// of killing the whole shell. `readline()` calls temporarily swap in their
/// own `SIGINT` handler for the duration of each call and restore whatever
/// was active beforehand when they return; since this is installed before
/// the first such call, that restore always lands back on this handler.
#[must_use]
pub const fn sig_interrupt_action() -> SigHandler {
    SigHandler::Handler(handle_sigint)
}

/// The trapped signals that have arrived since [`take_pending_traps`] last
/// drained them, set by [`handle_trapped`]: bit `n - 1` for signal `n`, so
/// all 64 Linux signals fit.
static PENDING_TRAPS: AtomicU64 = AtomicU64::new(0);

extern "C" fn handle_trapped(signum: core::ffi::c_int) {
    if let Ok(n) = u32::try_from(signum)
        && (1..=64).contains(&n)
    {
        PENDING_TRAPS.fetch_or(1 << (n - 1), Ordering::SeqCst);
    }
}

/// The disposition for a signal that has a `trap` action: note its arrival
/// for the shell to act on between commands.
#[must_use]
pub const fn trap_action() -> SigHandler {
    SigHandler::Handler(handle_trapped)
}

/// The lowest-numbered trapped signal that has arrived and not yet been
/// drained, leaving it pending.
pub fn first_pending_trap() -> Option<i32> {
    match PENDING_TRAPS.load(Ordering::SeqCst) {
        0 => None,
        bits => i32::try_from(bits.trailing_zeros() + 1).ok(),
    }
}

/// Drains the set of trapped signals that arrived since the last call, as a
/// bitmask with bit `n - 1` for signal `n`.
pub fn take_pending_traps() -> u64 {
    // Checked first so the common case, nothing pending, is a plain load:
    // this runs before every command.
    if PENDING_TRAPS.load(Ordering::Relaxed) == 0 {
        return 0;
    }
    PENDING_TRAPS.swap(0, Ordering::SeqCst)
}

/// Where [`handle_sigchld`] writes a byte for each `SIGCHLD`, or -1 for
/// nowhere.
static SIGCHLD_PIPE: AtomicI32 = AtomicI32::new(-1);

extern "C" fn handle_sigchld(_signum: core::ffi::c_int) {
    let fd = SIGCHLD_PIPE.load(Ordering::SeqCst);
    if fd >= 0 {
        // A write to a non-blocking pipe is async-signal-safe; a full pipe
        // already says there is something to look at.
        let _ = crate::fd::write_raw(fd, b"c");
    }
}

/// Makes every `SIGCHLD` write a byte to `fd`, a non-blocking pipe, or with
/// `None`, restores the default disposition. For waiting on children
/// alongside other descriptors.
pub fn notify_sigchld(fd: Option<RawFd>) {
    SIGCHLD_PIPE.store(fd.unwrap_or(-1), Ordering::SeqCst);
    let handler = if fd.is_some() {
        SigHandler::Handler(handle_sigchld)
    } else {
        SigHandler::Default
    };
    // SAFETY: the handler only stores to an atomic and writes to a pipe.
    let _ = unsafe { sigaction(Signal::CHILD, handler) };
}

/// Reports and clears whether `SIGINT` has arrived since the last call.
pub fn take_interrupted() -> bool {
    INTERRUPTED.swap(false, Ordering::SeqCst)
}

/// Restore default signal dispositions and unblock all signals in a forked child.
///
/// `interactive` gates `TTOU`/`TTIN`/`TSTP`/`INT`: the shell only ever
/// moves those away from their default disposition in the first place when
/// it is interactive or has job control on (`Shell::new`,
/// `Shell::set_monitor`), and callers pass exactly that, so
/// resetting them in a non-interactive shell's children is resetting
/// something that was never touched. That's not wrong, just 4 wasted
/// `rt_sigaction` calls per fork; found via `strace -c` on a command-
/// substitution-heavy benchmark, where they were a measurable share of
/// every fork's cost. `PIPE` (see `reset_sigpipe`'s own doc comment for
/// why it matters even non-interactively) and the mask reset stay
/// unconditional.
///
/// # Safety
/// Must only be called immediately after `fork`, in the child process, before
/// any allocator or async-signal-unsafe code runs.
#[inline]
pub unsafe fn restore_child_signals(interactive: bool) {
    let conditional: &[Signal] = if interactive {
        &[Signal::TTOU, Signal::TTIN, Signal::TSTP, Signal::INT]
    } else {
        &[]
    };
    for sig in conditional
        .iter()
        .copied()
        .chain([Signal::QUIT, Signal::PIPE])
    {
        // SAFETY: see this function's own `# Safety` doc: called only just
        // after `fork`, in the child, before any async-signal-unsafe code.
        let _ = unsafe { sigaction(sig, SigHandler::Default) };
    }
    // SAFETY: see this function's own `# Safety` doc.
    let _ = unsafe { unblock_all_signals() };
}

/// Resets `SIGPIPE` to its default (terminate) disposition for the shell's
/// own top-level process.
///
/// Rust's runtime ignores `SIGPIPE` at startup so
/// library code sees a normal `EPIPE` I/O error instead of dying silently,
/// which is convenient for typical CLI tools but wrong for a shell: every
/// other program on the system (and every other shell) lets a broken pipe
/// kill the writer outright, e.g. `while true; do echo hi; done | head -1`.
/// Left ignored, that same broken pipe instead reaches `println!`, which
/// treats any write error (including `EPIPE`) as fatal and panics. Forked
/// children pick up the same disposition via `restore_child_signals`; this
/// covers the cases that never fork (a single top-level builtin whose own
/// stdout is a pipe). Must run before the first write that could hit a
/// closed pipe, i.e. as early as possible in `main`.
pub fn reset_sigpipe() {
    // SAFETY: installs the default disposition (not a custom handler),
    // before any other threads exist.
    let _ = unsafe { sigaction(Signal::PIPE, SigHandler::Default) };
}
