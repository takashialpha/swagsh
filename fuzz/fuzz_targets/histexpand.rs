#![no_main]

use libfuzzer_sys::fuzz_target;

// History expansion parses `!` references, word designators and `:s/old/new/`
// modifiers out of a typed line by hand. A bad reference is an `Err`
// message, so any panic is a bug. The history is fuzzed too, since word
// designators index into the words of the event they select.
fuzz_target!(|input: (String, Vec<String>)| {
    let (line, history) = input;
    let _ = swagsh::repl::histexpand::expand(&line, &history);
});
