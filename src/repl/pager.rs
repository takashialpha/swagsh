//! Lists completions a page at a time, as bash does when there are too many
//! to show at once: it asks first when there are over [`ASK_ABOVE`], then
//! Space shows the next page, Enter the next line, and `q` stops. Ctrl-C
//! drops the line, as it does at the prompt.

use std::io::{self, Write};

use rustix::termios::{self, LocalModes, OptionalActions, SpecialCodeIndex};
use rustyline::completion::{Candidate, Pair};
use unicode_width::UnicodeWidthStr;

/// How many completions are listed without asking.
const ASK_ABOVE: usize = 100;

/// How the list is laid out: in `columns` of `width`, filled top to bottom.
struct Layout {
    width: usize,
    columns: usize,
    rows: usize,
}

impl Layout {
    fn new(candidates: &[Pair], screen_width: usize) -> Self {
        let widest = candidates
            .iter()
            .map(|c| c.display().width())
            .max()
            .unwrap_or(0);
        let width = (widest + 2).min(screen_width.max(1));
        let columns = (screen_width / width).max(1);
        Self {
            width,
            columns,
            rows: candidates.len().div_ceil(columns),
        }
    }

    /// Row `row` of the list.
    fn row(&self, candidates: &[Pair], row: usize) -> String {
        let mut out = String::new();
        for column in 0..self.columns {
            let Some(candidate) = candidates.get(column * self.rows + row) else {
                break;
            };
            let text = candidate.display();
            out.push_str(text);
            if candidates.len() > (column + 1) * self.rows + row {
                out.extend(std::iter::repeat_n(
                    ' ',
                    self.width.saturating_sub(text.width()),
                ));
            }
        }
        out
    }
}

/// The terminal's width and height.
fn screen() -> (usize, usize) {
    termios::tcgetwinsize(io::stdout()).map_or((80, 24), |size| {
        let or = |n: u16, default| if n == 0 { default } else { usize::from(n) };
        (or(size.ws_col, 80), or(size.ws_row, 24))
    })
}

/// Whether the line editor can list `candidates` itself: few enough not to
/// ask about, and to fit on the screen.
#[must_use]
pub fn fits(candidates: &[Pair]) -> bool {
    let (width, height) = screen();
    candidates.len() <= ASK_ABOVE && Layout::new(candidates, width).rows + 1 < height
}

/// Lists `candidates` from the start of a line, asking first if there are
/// many. `false` when Ctrl-C was pressed.
///
/// # Errors
///
/// Returns an error if the terminal cannot be read or written.
pub fn show(candidates: &[Pair]) -> io::Result<bool> {
    let (width, height) = screen();
    let layout = Layout::new(candidates, width);
    let _raw = RawMode::new()?;
    let mut out = io::stdout().lock();
    if candidates.len() > ASK_ABOVE {
        write!(
            out,
            "Display all {} possibilities? (y or n)",
            candidates.len()
        )?;
        out.flush()?;
        let answer = loop {
            match read_key()? {
                Key::Byte(b'y' | b'Y' | b' ' | b'\t') => break Some(true),
                Key::Byte(b'n' | b'N' | b'q' | b'Q' | 0x7f | 0x08 | 0x07) | Key::Escape => {
                    break Some(false);
                }
                Key::Byte(0x03) => break None,
                _ => {}
            }
        };
        writeln!(out)?;
        match answer {
            Some(true) => {}
            Some(false) => return Ok(true),
            None => return Ok(false),
        }
    }
    let page = height.saturating_sub(1).max(1);
    let mut until = page;
    for row in 0..layout.rows {
        if row == until {
            write!(out, "--More--")?;
            out.flush()?;
            let key = loop {
                match read_key()? {
                    Key::Byte(b' ' | b'y' | b'Y') => break Some(page),
                    Key::Byte(b'\r' | b'\n') => break Some(1),
                    Key::Byte(b'q' | b'Q' | b'n' | b'N' | 0x7f | 0x08 | 0x07) | Key::Escape => {
                        break Some(0);
                    }
                    Key::Byte(0x03) => break None,
                    _ => {}
                }
            };
            write!(out, "\r\x1b[K")?;
            match key {
                Some(0) => break,
                Some(more) => until += more,
                None => {
                    out.flush()?;
                    return Ok(false);
                }
            }
        }
        writeln!(out, "{}", layout.row(candidates, row))?;
    }
    out.flush()?;
    Ok(true)
}

enum Key {
    Byte(u8),
    Escape,
    /// A key that sends a sequence, such as an arrow.
    Other,
}

/// The next key pressed, in [`RawMode`]. The end of input counts as
/// Ctrl-C.
fn read_key() -> io::Result<Key> {
    let mut buf = [0u8; 16];
    let n = loop {
        match rustix::io::read(io::stdin(), &mut buf) {
            Ok(0) => return Ok(Key::Byte(0x03)),
            Ok(n) => break n,
            Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(e.into()),
        }
    };
    Ok(match (buf[0], n) {
        (0x1b, 1) => Key::Escape,
        (_, 1) => Key::Byte(buf[0]),
        _ => Key::Other,
    })
}

/// The terminal, sending each key as it is pressed while this lives.
struct RawMode {
    saved: termios::Termios,
}

impl RawMode {
    fn new() -> io::Result<Self> {
        let saved = termios::tcgetattr(io::stdin())?;
        let mut raw = saved.clone();
        raw.local_modes -= LocalModes::ICANON | LocalModes::ECHO | LocalModes::ISIG;
        raw.special_codes[SpecialCodeIndex::VMIN] = 1;
        raw.special_codes[SpecialCodeIndex::VTIME] = 0;
        termios::tcsetattr(io::stdin(), OptionalActions::Now, &raw)?;
        Ok(Self { saved })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = termios::tcsetattr(io::stdin(), OptionalActions::Now, &self.saved);
    }
}
