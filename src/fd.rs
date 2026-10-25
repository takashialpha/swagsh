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

/// Reports whether `fd` refers to a terminal.
///
/// Wrapped here rather than at the call site so the unsafe borrow of a raw
/// descriptor stays in this module with every other one (`test -t N` is the
/// only caller).
#[must_use]
pub fn fd_is_tty(fd: RawFd) -> bool {
    // SAFETY: the descriptor is only borrowed for the `isatty` query; nothing
    // here takes ownership of it or closes it.
    rustix::termios::isatty(unsafe { BorrowedFd::borrow_raw(fd) })
}

/// Closes `fd` only if it is currently open.
///
/// [`close_raw`] hands the descriptor to an `OwnedFd`, and that drop asserts
/// the underlying `close` succeeded: closing a descriptor that was never
/// open (`exec 3>&-` in a shell with no fd 3) aborts the whole process with
/// "IO Safety violation: owned file descriptor already closed". A shell
/// wants that to be a quiet no-op, so probe first.
pub fn close_if_open(fd: RawFd) {
    // SAFETY: borrowed only for the fcntl probe; ownership is not taken
    // here, and `close_raw` below re-derives its own owned handle.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    if rustix::io::fcntl_getfd(borrowed).is_ok() {
        close_raw(fd);
    }
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
/// Returns an error if the underlying `read` syscall fails, `EBADF` for a
/// descriptor that is not open.
pub fn read_raw(fd: RawFd, buf: &mut [u8]) -> rustix::io::Result<usize> {
    // SAFETY: the descriptor is only borrowed for the one `read` call, and
    // nothing closes it meanwhile; one that is not open just fails.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    read(borrowed, buf)
}

/// A copy of `fd` on the lowest free descriptor from `min` up, which
/// programs the shell runs inherit.
///
/// # Errors
///
/// Returns the `fcntl(2)` errno.
pub fn dup_at_least(fd: RawFd, min: RawFd) -> rustix::io::Result<RawFd> {
    // SAFETY: as in `read_raw`.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    let fd = fcntl_dupfd_cloexec(borrowed, min)?;
    rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::empty())?;
    Ok(fd.into_raw_fd())
}

/// Marks `fd` close-on-exec, so programs the shell runs do not inherit it.
pub fn set_cloexec(fd: RawFd) {
    // SAFETY: as in `read_raw`.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    let _ = rustix::io::fcntl_setfd(borrowed, rustix::io::FdFlags::CLOEXEC);
}

/// Moves `fd`'s file offset back by `n` bytes. False if it cannot seek (a
/// pipe or terminal).
pub fn seek_back(fd: RawFd, n: usize) -> bool {
    // SAFETY: as in `read_raw`.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    let Ok(n) = i64::try_from(n) else {
        return false;
    };
    fs::seek(borrowed, fs::SeekFrom::Current(-n)).is_ok()
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

/// Opens `path` for `>` under `set -C`: an existing regular file is an
/// error rather than being truncated, while anything else that already
/// exists (`/dev/null`, a FIFO, ...) is opened as usual.
///
/// # Errors
///
/// Returns an error if `path` is an existing regular file, or if the
/// underlying `open` syscall fails.
pub fn open_write_noclobber(path: &std::path::Path) -> Result<RawFd> {
    let mode = Mode::RUSR | Mode::WUSR | Mode::RGRP | Mode::WGRP | Mode::ROTH | Mode::WOTH;
    match fs::open(path, OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL, mode) {
        Ok(fd) => Ok(fd.into_raw_fd()),
        Err(rustix::io::Errno::EXIST) if !path.is_file() => {
            Ok(fs::open(path, OFlags::WRONLY, Mode::empty())?.into_raw_fd())
        }
        Err(rustix::io::Errno::EXIST) => anyhow::bail!("cannot overwrite existing file"),
        Err(e) => Err(e.into()),
    }
}

/// Opens `path` for reading and writing, creating it if it does not exist.
///
/// # Errors
///
/// Returns an error if the underlying `open` syscall fails.
pub fn open_read_write(path: &std::path::Path) -> Result<RawFd> {
    let fd = fs::open(
        path,
        OFlags::RDWR | OFlags::CREATE,
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
/// Returns an error if the underlying `fcntl` syscall fails, `BADF` when
/// `fd` is not open.
pub fn dup_save(fd: RawFd) -> rustix::io::Result<RawFd> {
    // SAFETY: borrowed only for the `fcntl` call, which reports `BADF`
    // rather than misbehaving if `fd` is not open.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    let duped = fcntl_dupfd_cloexec(borrowed, 10)?;
    Ok(duped.into_raw_fd())
}

/// Saves a copy of each fd in `fds`, returning `(original, saved)` pairs. A
/// descriptor that is not open is recorded as `None`, so restoring closes
/// whatever a redirect opened there.
///
/// # Errors
///
/// Returns an error if duplicating an open descriptor fails; any
/// descriptors already saved are closed before returning.
pub fn save_fds(fds: &[RawFd]) -> Result<Vec<(RawFd, Option<RawFd>)>> {
    let mut saved = Vec::with_capacity(fds.len());
    for &fd in fds {
        match dup_save(fd) {
            Ok(saved_fd) => saved.push((fd, Some(saved_fd))),
            Err(rustix::io::Errno::BADF) => saved.push((fd, None)),
            Err(e) => {
                for (_, s) in saved {
                    if let Some(s) = s {
                        close_raw(s);
                    }
                }
                return Err(e.into());
            }
        }
    }
    Ok(saved)
}

/// Restores each `(original, saved)` fd pair produced by [`save_fds`],
/// closing the saved copy afterwards, or closing `original` if it was not
/// open when saved.
///
/// # Errors
///
/// Returns an error if restoring any descriptor fails.
pub fn restore_fds(saved: Vec<(RawFd, Option<RawFd>)>) -> Result<()> {
    for (original, saved_fd) in saved {
        match saved_fd {
            Some(saved_fd) => {
                dup2_raw(saved_fd, original)?;
                close_raw(saved_fd);
            }
            None => close_if_open(original),
        }
    }
    Ok(())
}
