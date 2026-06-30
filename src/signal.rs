use rustix::process::Signal;
use rustix::runtime::{
    How, KERNEL_SIG_DFL, KernelSigSet, KernelSigaction, KernelSigactionFlags, kernel_sig_ign,
    kernel_sigaction, kernel_sigprocmask,
};

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
