// `echo` can't go through clap: clap's whole model is "recognize known
// flags, error on anything else that starts with `-`," but `echo`'s actual
// rule is the opposite: a `-`-led word that *isn't* a valid n/e/E cluster
// is not an error, it's the first word of the output (`echo -x` prints
// `-x` literally). clap also hard-codes `--` as an unconditional
// end-of-options marker with no way to opt out, whereas `echo` doesn't
// treat `--` as special at all (`echo -- foo` prints `-- foo`, not `foo`).
// Both are load-bearing, tested differences from every clap-backed
// builtin's error-on-unknown-flag convention, not an oversight.

use anyhow::Result;

use crate::eval::Shell;
use crate::expand::unescape;
use crate::jobs::ExitStatus;

/// `echo` treats a word as an option cluster only if it starts with
/// `-` and every character after that is `n`/`e`/`E` (so `-ne`, `-en`, and
/// repeats like `-nnee` all count, but `-x` or `-en3` don't): the first
/// word that doesn't qualify, option-looking or not, ends option scanning
/// and starts the output instead.
fn is_echo_option(arg: &str) -> bool {
    arg.len() > 1 && arg.starts_with('-') && arg[1..].chars().all(|c| matches!(c, 'n' | 'e' | 'E'))
}

#[allow(clippy::unnecessary_wraps)] // required by BuiltinFn signature
pub fn builtin_echo(shell: &mut Shell, args: &[&str]) -> Result<ExitStatus> {
    let mut interpret_escapes = false;
    let mut no_newline = false;
    let mut start = 0;
    for arg in args {
        if !is_echo_option(arg) {
            break;
        }
        for c in arg[1..].chars() {
            match c {
                'n' => no_newline = true,
                'e' => interpret_escapes = true,
                'E' => interpret_escapes = false,
                _ => unreachable!("is_echo_option only admits n/e/E"),
            }
        }
        start += 1;
    }
    let output = args[start..].join(" ");
    let (output, stopped_early) = if interpret_escapes {
        unescape(&output)
    } else {
        (output, false)
    };
    if no_newline || stopped_early {
        print!("{output}");
        shell.note_stdout(&output);
    } else {
        println!("{output}");
        shell.note_stdout("\n");
    }
    Ok(ExitStatus::SUCCESS)
}
