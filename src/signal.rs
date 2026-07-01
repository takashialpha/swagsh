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

/// The interactive shell's own disposition for `SIGINT`, installed once at
/// startup (see `Shell::new`) so a `^C` typed while a builtin loop or
/// function is running (i.e. no forked child to absorb the default
/// terminate action) sets a flag the interpreter can notice instead of
/// killing the whole shell. `readline()` calls temporarily swap in their
/// own `SIGINT` handler for the duration of each call and restore whatever
/// was active beforehand when they return; since this is installed before
/// the first such call, that restore always lands back on this handler.
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

pub fn sig_ign_action() -> KernelSigaction {
    KernelSigaction {
        sa_handler_kernel: kernel_sig_ign(),
        sa_flags: KernelSigactionFlags::empty(),
        sa_mask: KernelSigSet::empty(),
        ..Default::default()
    }
}

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
/// # Safety
/// Must only be called immediately after `fork`, in the child process, before
/// any allocator or async-signal-unsafe code runs.
#[inline]
pub unsafe fn restore_child_signals() {
    let dfl = sig_dfl_action();
    let _ = unsafe { kernel_sigaction(Signal::TTOU, Some(dfl.clone())) };
    let _ = unsafe { kernel_sigaction(Signal::TTIN, Some(dfl.clone())) };
    let _ = unsafe { kernel_sigaction(Signal::TSTP, Some(dfl.clone())) };
    let _ = unsafe { kernel_sigaction(Signal::INT, Some(dfl.clone())) };
    let _ = unsafe { kernel_sigaction(Signal::QUIT, Some(dfl)) };
    let _ = unsafe { kernel_sigprocmask(How::SETMASK, Some(&KernelSigSet::empty())) };
}
