//! The order pathname expansion sorts its results in: the collating sequence
//! of the current locale, as POSIX specifies, which `LC_ALL`, `LC_COLLATE`
//! and `LANG` choose in that order of precedence.
//!
//! The shell's own variables decide, not the environment it started with, so
//! `LC_ALL=C` set in a script takes effect at once. The C library's locale
//! is switched only when a sort first needs it, so a script that never
//! globs, or that runs in the C locale, never loads locale data.

use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::ffi::CString;

use crate::sys::{set_collation, strcoll};

/// A collation the variables asked for, not yet in effect.
#[derive(Clone, Copy)]
enum Pending {
    Nothing,
    /// Plain byte order: the C locale, or another that collates by code
    /// point, which for UTF-8 is byte order.
    Bytes,
    /// The locale in `REQUESTED`.
    Locale,
}

thread_local! {
    static PENDING: Cell<Pending> = const { Cell::new(Pending::Nothing) };
    /// The locale last selected by name, its buffer reused from one
    /// selection to the next: scripts switch locale variables in loops.
    static REQUESTED: RefCell<String> = const { RefCell::new(String::new()) };
    /// The locale the C library's collation was last switched to.
    static APPLIED: RefCell<String> = const { RefCell::new(String::new()) };
    /// Byte order is in effect, rather than the C library's collation.
    static BYTE_ORDER: Cell<bool> = const { Cell::new(true) };
}

/// Makes `locale` the one later sorts follow.
///
/// Before a named locale, a change still pending is applied first, so that
/// an unknown one leaves the order the previous one set, as it leaves the C
/// library's.
pub fn select(locale: &str) {
    if matches!(locale, "" | "C" | "POSIX" | "C.UTF-8" | "C.utf8") {
        PENDING.set(Pending::Bytes);
        return;
    }
    apply_pending();
    REQUESTED.with_borrow_mut(|r| {
        r.clear();
        r.push_str(locale);
    });
    PENDING.set(Pending::Locale);
}

fn apply_pending() {
    match PENDING.replace(Pending::Nothing) {
        Pending::Nothing => {}
        Pending::Bytes => BYTE_ORDER.set(true),
        Pending::Locale => REQUESTED.with_borrow(|requested| {
            APPLIED.with_borrow_mut(|applied| {
                if applied == requested {
                    BYTE_ORDER.set(false);
                } else if set_collation(requested) {
                    requested.clone_into(applied);
                    BYTE_ORDER.set(false);
                }
            });
        }),
    }
}

/// Sorts `names` in the current locale's collating sequence. Names that
/// collate equal keep byte order between them, so the result is the same
/// on every run.
pub fn sort(names: &mut [String]) {
    if names.len() < 2 {
        return;
    }
    apply_pending();
    if BYTE_ORDER.get() {
        names.sort_unstable();
        return;
    }
    let mut keyed: Vec<(Option<CString>, String)> = names
        .iter_mut()
        .map(|n| {
            let name = std::mem::take(n);
            (CString::new(name.as_str()).ok(), name)
        })
        .collect();
    keyed.sort_by(|(ka, a), (kb, b)| {
        let order = match (ka, kb) {
            (Some(ka), Some(kb)) => strcoll(ka, kb),
            _ => Ordering::Equal,
        };
        order.then_with(|| a.cmp(b))
    });
    for (slot, (_, name)) in names.iter_mut().zip(keyed) {
        *slot = name;
    }
}

/// Compares `a` and `b` in the current locale's collating sequence, as
/// `[[ a < b ]]` does.
#[must_use]
pub fn compare(a: &str, b: &str) -> Ordering {
    apply_pending();
    if BYTE_ORDER.get() {
        return a.cmp(b);
    }
    match (CString::new(a), CString::new(b)) {
        (Ok(x), Ok(y)) => strcoll(&x, &y).then_with(|| a.cmp(b)),
        _ => a.cmp(b),
    }
}
