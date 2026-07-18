use std::os::fd::{FromRawFd, IntoRawFd};

use anyhow::Result;
use rustix::fd::{BorrowedFd, OwnedFd, RawFd};
use rustix::fs::{self, Mode, OFlags};
use rustix::io::{dup2, fcntl_dupfd_cloexec, read, write};
use rustix::pipe::pipe;

/// Duplicates `oldfd` onto `newfd` (`dup2` semantics).
///
/// # Errors
///
/// Returns an error if the underlying `dup2` syscall fails.
#[inline]
pub fn dup2_raw(oldfd: RawFd, newfd: RawFd) -> rustix::io::Result<()> {
    // SAFETY: both fds are valid at every call site.
    let src = unsafe { BorrowedFd::borrow_raw(oldfd) };
    // SAFETY: newfd is a valid fd owned by the caller at every call site; the
    // resulting OwnedFd is wrapped in ManuallyDrop below so it is never closed
    // here, since the caller retains ownership of that fd slot.
    let dst = unsafe { OwnedFd::from_raw_fd(newfd) };
    let mut dst = std::mem::ManuallyDrop::new(dst);
    dup2(src, &mut dst)
}

#[inline]
pub fn close_raw(fd: RawFd) {
    // SAFETY: transferring ownership to OwnedFd so it closes on drop.
    let _ = unsafe { OwnedFd::from_raw_fd(fd) };
}

/// Creates a new pipe, returning the raw `(read, write)` file descriptors.
///
/// # Errors
///
/// Returns an error if the underlying `pipe` syscall fails.
pub fn raw_pipe() -> rustix::io::Result<(RawFd, RawFd)> {
    let (r, w) = pipe()?;
    Ok((r.into_raw_fd(), w.into_raw_fd()))
}

/// Reads from `fd` into `buf`.
///
/// # Errors
///
/// Returns an error if the underlying `read` syscall fails.
pub fn read_raw(fd: RawFd, buf: &mut [u8]) -> rustix::io::Result<usize> {
    // SAFETY: fd is a valid open pipe read-end at every call site.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    read(borrowed, buf)
}

/// Writes `buf` to `fd`.
///
/// # Errors
///
/// Returns an error if the underlying `write` syscall fails.
pub fn write_raw(fd: RawFd, buf: &[u8]) -> rustix::io::Result<usize> {
    // SAFETY: fd is a valid open pipe write-end at every call site.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    write(borrowed, buf)
}

/// Opens `path` for writing, truncating unless `append` is set.
///
/// # Errors
///
/// Returns an error if the underlying `open` syscall fails.
pub fn open_write(path: &std::path::Path, append: bool) -> Result<RawFd> {
    let mut opts = OFlags::WRONLY | OFlags::CREATE;
    opts |= if append {
        OFlags::APPEND
    } else {
        OFlags::TRUNC
    };
    let fd = fs::open(
        path,
        opts,
        Mode::RUSR | Mode::WUSR | Mode::RGRP | Mode::WGRP | Mode::ROTH | Mode::WOTH,
    )?;
    Ok(fd.into_raw_fd())
}

/// Opens `path` for reading.
///
/// # Errors
///
/// Returns an error if the underlying `open` syscall fails.
pub fn open_read(path: &std::path::Path) -> Result<RawFd> {
    let fd = fs::open(path, OFlags::RDONLY, Mode::empty())?;
    Ok(fd.into_raw_fd())
}

/// Duplicates `fd` onto a fresh, close-on-exec descriptor.
///
/// # Errors
///
/// Returns an error if the underlying `fcntl` syscall fails.
pub fn dup_save(fd: RawFd) -> rustix::io::Result<RawFd> {
    // SAFETY: fd is a valid, open descriptor at every call site.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    let duped = fcntl_dupfd_cloexec(borrowed, 10)?;
    Ok(duped.into_raw_fd())
}

/// Saves a copy of each fd in `fds`, returning `(original, saved)` pairs.
///
/// # Errors
///
/// Returns an error if duplicating any descriptor fails; any descriptors
/// already saved are closed before returning.
pub fn save_fds(fds: &[RawFd]) -> Result<Vec<(RawFd, RawFd)>> {
    let mut saved = Vec::with_capacity(fds.len());
    for &fd in fds {
        match dup_save(fd) {
            Ok(saved_fd) => saved.push((fd, saved_fd)),
            Err(e) => {
                for (_, s) in saved {
                    close_raw(s);
                }
                return Err(e.into());
            }
        }
    }
    Ok(saved)
}

/// Restores each `(original, saved)` fd pair produced by [`save_fds`],
/// closing the saved copy afterwards.
///
/// # Errors
///
/// Returns an error if restoring any descriptor fails.
pub fn restore_fds(saved: Vec<(RawFd, RawFd)>) -> Result<()> {
    for (original, saved_fd) in saved {
        dup2_raw(saved_fd, original)?;
        close_raw(saved_fd);
    }
    Ok(())
}
