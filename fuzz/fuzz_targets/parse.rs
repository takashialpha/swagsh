#![no_main]

use libfuzzer_sys::fuzz_target;

// `parser::parse` is the whole lex+parse pipeline: pure, deterministic,
// and the one invariant that matters is "never panics, for any input" (a
// real syntax error is an `Err`, not a crash). Skips inputs that aren't
// valid UTF-8 rather than lossily converting them, since `parse` only
// ever sees `&str` in the real shell too (word bytes come from a UTF-8
// source file or terminal line).
fuzz_target!(|data: &[u8]| {
    if let Ok(src) = std::str::from_utf8(data) {
        let _ = swagsh::parser::parse(src);
    }
});
