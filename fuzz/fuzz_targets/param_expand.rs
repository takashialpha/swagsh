#![no_main]

use libfuzzer_sys::fuzz_target;

// Bundles the smaller parameter-expansion helpers behind `${var...}`
// syntax into one target, since none of them is individually large enough
// to be worth a separate corpus: `glob_match` (`case`/`${var#pat}`'s
// pattern matcher, genuinely two independent string inputs, hence
// `arbitrary` instead of a single `&str`), `parse_param_op` (parses
// `${var#pat}`/`${var:-word}`/... apart), and `strip_prefix`/`strip_suffix`
// (the `#`/`##`/`%`/`%%` operators themselves).
fuzz_target!(|input: (String, String)| {
    let (a, b) = input;

    let _ = swagsh::expand::glob_match(&a, &b);
    let _ = swagsh::expand::parse_param_op(&a);
    let _ = swagsh::expand::strip_prefix(&a, &b, false);
    let _ = swagsh::expand::strip_prefix(&a, &b, true);
    let _ = swagsh::expand::strip_suffix(&a, &b, false);
    let _ = swagsh::expand::strip_suffix(&a, &b, true);
});
