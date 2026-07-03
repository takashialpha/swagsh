//! Shell-style formatting for OS-level errors.
//!
//! `std::io::Error` and `rustix::io::Errno` both implement `Display` by
//! pairing the `strerror(3)` text with the raw errno, e.g.
//! `"No such file or directory (os error 2)"`. That's the right call for a
//! general-purpose Rust `Display` impl, but it reads oddly on a shell's
//! stderr: every Unix shell prints just the `strerror(3)` text
//! (`swagsh: nope: No such file or directory`) and never surfaces the
//! numeric errno to the user. This module keeps swagsh's error output on
//! that convention.

/// Renders an OS error the way a Unix shell does: the bare `strerror(3)`
/// message, without Rust's `" (os error N)"` annotation.
///
/// Accepts anything `Display`-able (`std::io::Error`, `rustix::io::Errno`,
/// `rustyline::error::ReadlineError`, ...) since they all funnel through the
/// same `"<message> (os error N)"` shape. Errors that don't carry an OS
/// errno (parse errors, custom messages) pass through unchanged, so it's
/// always safe to wrap an error in `strerror()` at a formatting call site.
///
/// ```ignore
/// let e = std::io::Error::from_raw_os_error(2);
/// assert_eq!(strerror(e), "No such file or directory");
/// ```
pub fn strerror(err: impl std::fmt::Display) -> String {
    let msg = err.to_string();
    match msg.find(" (os error ") {
        Some(idx) => msg[..idx].to_owned(),
        None => msg,
    }
}

/// Prints a user-facing error the way the rest of swagsh does: `swagsh:
/// <message>`, with any `(os error N)` suffix stripped via [`strerror`].
///
/// Every `Result` error that bubbles up through the interpreter (a bad
/// redirect target, a failed `exec`, a builtin's own error, ...) ends up
/// printed at some catch point; there are over a dozen of those across
/// `eval`, and any one of them might be displaying an error that wraps an
/// OS error several `?`s downstream. Routing them all through here instead
/// of a bare `eprintln!("swagsh: {e}")` means that's handled once, instead
/// of relying on every call site to remember `strerror()`.
pub fn emit(err: impl std::fmt::Display) {
    eprintln!("swagsh: {}", strerror(err));
}
