#![no_main]

use libfuzzer_sys::fuzz_target;

// `eval_arith` backs `$(( ))`. Unlike `parser::parse`, it doesn't even
// return a `Result`: there's no error path at all, so *any* panic here
// (integer overflow, division by zero, deep paren-nesting stack overflow,
// ...) is unambiguously a real bug, not a debatable "should this have
// been an Err instead" question.
fuzz_target!(|data: &[u8]| {
    if let Ok(expr) = std::str::from_utf8(data) {
        let _ = swagsh::expand::eval_arith(expr);
    }
});
