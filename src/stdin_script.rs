//! Commands read from standard input by a shell that is not interactive
//! (`sh < file`, `cmd | sh`, `sh -s`).
//!
//! Input is read a line at a time and each complete command runs before
//! the next is read, so a command that reads standard input itself (`read`,
//! `cat`) gets the lines after it, as POSIX requires.

use rustix::fs::{SeekFrom, seek};
use rustix::io::{Errno, read};

use crate::errfmt::emit;
use crate::eval::{Shell, SyntaxError};

/// Reads standard input up to each newline and no further.
struct LineReader {
    /// A regular file, which can be read in blocks and seeked back to the
    /// end of the line. A pipe or terminal is read a byte at a time.
    seekable: bool,
}

impl LineReader {
    fn new() -> Self {
        Self {
            seekable: seek(std::io::stdin(), SeekFrom::Current(0)).is_ok(),
        }
    }

    /// The next line, with its newline, or `None` at the end of input.
    fn line(&self) -> Option<Vec<u8>> {
        let mut line = Vec::new();
        let mut buf = [0u8; 128];
        let block = if self.seekable { 128 } else { 1 };
        loop {
            let n = match read(std::io::stdin(), &mut buf[..block]) {
                Ok(0) => break,
                Ok(n) => n,
                Err(Errno::INTR) => continue,
                Err(_) => break,
            };
            if let Some(end) = buf[..n].iter().position(|&b| b == b'\n') {
                line.extend_from_slice(&buf[..=end]);
                let unread = i64::try_from(n - end - 1).unwrap_or(0);
                if unread > 0 {
                    let _ = seek(std::io::stdin(), SeekFrom::Current(-unread));
                }
                return Some(line);
            }
            line.extend_from_slice(&buf[..n]);
        }
        (!line.is_empty()).then_some(line)
    }
}

/// Runs the commands on standard input, then exits with the status of the
/// last.
pub fn run(shell: &mut Shell) -> ! {
    shell.stdin_input = true;
    let reader = LineReader::new();
    let mut src = String::new();
    // The line `src` starts on, for `$LINENO` and error messages.
    let mut first_line = 1;
    while let Some(line) = reader.line() {
        src.push_str(&String::from_utf8_lossy(&line));
        let parsed = crate::parser::parse_with_aliases_at(&src, shell.env.aliases(), first_line);
        let program = match parsed {
            Err(e) if e.incomplete => continue,
            Err(error) => {
                let context = String::new();
                fail(shell, &SyntaxError { context, error }.into())
            }
            Ok(program) => program,
        };
        if shell.verbose {
            eprint!("{src}");
        }
        first_line += src.matches('\n').count();
        src.clear();
        if let Err(e) = shell.run_program(&program) {
            fail(shell, &e);
        }
    }
    // What is left ends inside a construct, such as a here-document
    // delimited by the end of input. The newlines before it give it its
    // line numbers.
    if !src.is_empty() {
        let src = "\n".repeat(first_line - 1) + &src;
        if let Err(e) = shell.run_source(&src, "") {
            fail(shell, &e);
        }
    }
    let status = shell.last_status.0;
    shell.exit(status)
}

/// Reports `e` and exits, as a script does on an error that ends it.
fn fail(shell: &mut Shell, e: &anyhow::Error) -> ! {
    emit(e);
    shell.exit(if e.is::<SyntaxError>() { 2 } else { 1 })
}
