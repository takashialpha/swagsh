use std::sync::atomic::{AtomicBool, Ordering};

use rustix::process::Signal;
use rustix::runtime::{
    How, KERNEL_SIG_DFL, KernelSigSet, KernelSigaction, KernelSigactionFlags, kernel_sig_ign,
    kernel_sigaction, kernel_sigprocmask,
};

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
pub fn sig_interrupt_action() -> KernelSigaction {
    KernelSigaction {
        sa_handler_kernel: Some(handle_sigint),
        sa_flags: KernelSigactionFlags::empty(),
        sa_mask: KernelSigSet::empty(),
        ..Default::default()
    }
}

/// Reports and clears whether `SIGINT` has arrived since the last call.
pub fn take_interrupted() -> bool {
    INTERRUPTED.swap(false, Ordering::SeqCst)
}

#[must_use]
pub fn sig_ign_action() -> KernelSigaction {
    KernelSigaction {
        sa_handler_kernel: kernel_sig_ign(),
        sa_flags: KernelSigactionFlags::empty(),
        sa_mask: KernelSigSet::empty(),
        ..Default::default()
    }
}

#[must_use]
pub fn sig_dfl_action() -> KernelSigaction {
    KernelSigaction {
        sa_handler_kernel: KERNEL_SIG_DFL,
        sa_flags: KernelSigactionFlags::empty(),
        sa_mask: KernelSigSet::empty(),
        ..Default::default()
    }
}

/// Restore default signal dispositions and unblock all signals in a forked child.
///
/// `interactive` gates `TTOU`/`TTIN`/`TSTP`/`INT`: `Shell::new` only ever
/// moves those away from their default disposition in the first place when
/// `interactive` is true (see its own `if interactive { ... }` block), so
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
    let dfl = sig_dfl_action();
    if interactive {
        // SAFETY: see this function's own `# Safety` doc: called only just
        // after `fork`, in the child, before any async-signal-unsafe code.
        let _ = unsafe { kernel_sigaction(Signal::TTOU, Some(dfl.clone())) };
        // SAFETY: see this function's own `# Safety` doc.
        let _ = unsafe { kernel_sigaction(Signal::TTIN, Some(dfl.clone())) };
        // SAFETY: see this function's own `# Safety` doc.
        let _ = unsafe { kernel_sigaction(Signal::TSTP, Some(dfl.clone())) };
        // SAFETY: see this function's own `# Safety` doc.
        let _ = unsafe { kernel_sigaction(Signal::INT, Some(dfl.clone())) };
    }
    // SAFETY: see this function's own `# Safety` doc.
    let _ = unsafe { kernel_sigaction(Signal::QUIT, Some(dfl.clone())) };
    // SAFETY: see this function's own `# Safety` doc.
    let _ = unsafe { kernel_sigaction(Signal::PIPE, Some(dfl)) };
    // SAFETY: see this function's own `# Safety` doc.
    let _ = unsafe { kernel_sigprocmask(How::SETMASK, Some(&KernelSigSet::empty())) };
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
    unsafe {
        let _ = kernel_sigaction(Signal::PIPE, Some(sig_dfl_action()));
    }
}
