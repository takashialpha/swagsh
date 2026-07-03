#![no_main]

use libfuzzer_sys::fuzz_target;

// Backs `echo -e`/`printf`'s backslash-escape expansion (`\xHH`, `\uHHHH`,
// `\0NNN`, ...): several of those forms consume a bounded run of following
// hex/octal digits by hand, exactly the kind of manual index-walking that
// panics on malformed input if a boundary check is off by one.
fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = swagsh::expand::unescape(s);
    }
});
