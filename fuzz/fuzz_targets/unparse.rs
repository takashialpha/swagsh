#![no_main]

use libfuzzer_sys::fuzz_target;
use swagsh::ast::{Command, Program};

// `declare -f` and `type` print a function by unparsing its body, and that
// text must parse back to the same commands: printed and parsed again, it
// prints the same. Line numbers differ between the two parses, so the
// texts are compared rather than the trees.
fuzz_target!(|data: &[u8]| {
    let Ok(body) = std::str::from_utf8(data) else {
        return;
    };
    let Some(first) = unparse(&format!("f() {{\n{body}\n}}\n")) else {
        return;
    };
    // Brace expansion can make a short input print as megabytes (the shell
    // caps it at 64 MiB), and parsing that again under the sanitizer would
    // pass libFuzzer's memory limit while testing nothing new. Text this
    // long is no different in kind from shorter text, so it is skipped.
    if first.len() > 1 << 20 {
        return;
    }
    let second = unparse(&first);
    assert_eq!(second.as_deref(), Some(first.as_str()), "printed as:\n{first}");
});

/// The definition of `f` printed back, if `src` parses to it.
fn unparse(src: &str) -> Option<String> {
    let Program { body } = swagsh::parser::parse(src).ok()?;
    let [list] = body.as_slice() else { return None };
    let [item] = list.items.as_slice() else { return None };
    let [Command::FunctionDef(def)] = item.command.commands.as_slice() else {
        return None;
    };
    Some(swagsh::unparse::function(&def.name, &def.body))
}
