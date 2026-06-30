use std::os::fd::{FromRawFd, IntoRawFd};

use anyhow::{Result, anyhow};
use rustix::fd::{BorrowedFd, OwnedFd, RawFd};
use rustix::fs::{self, Mode, OFlags};
use rustix::io::{dup2, fcntl_dupfd_cloexec, read, write};
use rustix::pipe::pipe;

#[inline]
pub fn dup2_raw(oldfd: RawFd, newfd: RawFd) -> rustix::io::Result<()> {
    // SAFETY: both fds are valid at every call site.
    let src = unsafe { BorrowedFd::borrow_raw(oldfd) };
    let mut dst = unsafe { OwnedFd::from_raw_fd(newfd) };
    let result = dup2(src, &mut dst);
    // Do NOT let dst drop: the caller owns that fd slot.
    std::mem::forget(dst);
    result
}

#[inline]
pub fn close_raw(fd: RawFd) {
    // SAFETY: transferring ownership to OwnedFd so it closes on drop.
    let _ = unsafe { OwnedFd::from_raw_fd(fd) };
}

pub fn raw_pipe() -> rustix::io::Result<(RawFd, RawFd)> {
    let (r, w) = pipe()?;
    Ok((r.into_raw_fd(), w.into_raw_fd()))
}

pub fn read_raw(fd: RawFd, buf: &mut [u8]) -> rustix::io::Result<usize> {
    // SAFETY: fd is a valid open pipe read-end at every call site.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    read(borrowed, buf)
}

pub fn write_raw(fd: RawFd, buf: &[u8]) -> rustix::io::Result<usize> {
    // SAFETY: fd is a valid open pipe write-end at every call site.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    write(borrowed, buf)
}

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

pub fn open_read(path: &std::path::Path) -> Result<RawFd> {
    let fd = fs::open(path, OFlags::RDONLY, Mode::empty())?;
    Ok(fd.into_raw_fd())
}

pub fn dup_save(fd: RawFd) -> rustix::io::Result<RawFd> {
    // SAFETY: fd is a valid, open descriptor at every call site.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    let duped = fcntl_dupfd_cloexec(borrowed, 10)?;
    Ok(duped.into_raw_fd())
}

pub fn save_fds(fds: &[RawFd]) -> Result<Vec<(RawFd, RawFd)>> {
    let mut saved = Vec::with_capacity(fds.len());
    for &fd in fds {
        match dup_save(fd) {
            Ok(saved_fd) => saved.push((fd, saved_fd)),
            Err(e) => {
                for (_, s) in saved {
                    close_raw(s);
                }
                return Err(anyhow!(e));
            }
        }
    }
    Ok(saved)
}

pub fn restore_fds(saved: Vec<(RawFd, RawFd)>) -> Result<()> {
    for (original, saved_fd) in saved {
        dup2_raw(saved_fd, original).map_err(|e| anyhow!(e))?;
        close_raw(saved_fd);
    }
    Ok(())
}
