#![no_main]

use libfuzzer_sys::fuzz_target;

// `eval_arith` backs `$(( ))`. Malformed input, division by zero and deep
// nesting all have an `Err` path, so *any* panic here (integer overflow,
// stack overflow, ...) is unambiguously a real bug.
fuzz_target!(|data: &[u8]| {
    if let Ok(expr) = std::str::from_utf8(data) {
        let _ = swagsh::expand::eval_arith(expr);
    }
});
