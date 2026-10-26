#![no_main]

use libfuzzer_sys::fuzz_target;

// Brace expansion runs on every word the parser lexes, and splits ranges
// (`{a..z..3}`) and nested lists apart by hand. Its size limits turn an
// explosive word into `None`, so a panic, a hang or running out of memory
// is a bug.
fuzz_target!(|data: &[u8]| {
    if let Ok(word) = std::str::from_utf8(data) {
        let _ = swagsh::brace::expand(word);
    }
});
