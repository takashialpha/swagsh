<div align="center">

<pre>
███████╗██╗    ██╗ █████╗  ██████╗ ███████╗██╗  ██╗
██╔════╝██║    ██║██╔══██╗██╔════╝ ██╔════╝██║  ██║
███████╗██║ █╗ ██║███████║██║  ███╗███████╗███████║
╚════██║██║███╗██║██╔══██║██║   ██║╚════██║██╔══██║
███████║╚███╔███╔╝██║  ██║╚██████╔╝███████║██║  ██║
╚══════╝ ╚══╝╚══╝ ╚═╝  ╚═╝ ╚═════╝ ╚══════╝╚═╝  ╚═╝
</pre>

**A fast, minimal, modern Linux shell. Named after swag, slang for stylish flair.**

[![Website](https://img.shields.io/badge/website-takashialpha.com%2Fswagsh-64b4ff?style=flat-square&labelColor=161616)](https://takashialpha.com/swagsh)
[![crates.io](https://img.shields.io/crates/v/swagsh?style=flat-square&color=64b4ff&labelColor=161616)](https://crates.io/crates/swagsh)
[![AUR](https://img.shields.io/aur/version/swagsh?style=flat-square&color=64b4ff&labelColor=161616)](https://aur.archlinux.org/packages/swagsh)
[![License](https://img.shields.io/crates/l/swagsh?style=flat-square&color=64b4ff&labelColor=161616)](LICENSE)

</div>

---

## Features

- **Shell grammar:** pipelines, redirections, control flow, functions, subshells, and here-documents.
- **Expansions:** variable, parameter, tilde, glob, and command substitution.
- **Tab completion:** builtins, aliases, executables and filenames, out of the box.
- **Job control:** background jobs, foreground and background switching, stopping and signalling.
- **Prompt:** customisable via `$PS1` with `\w`, `\W`, `\u`, `\h`, `\e`, `\[`/`\]` and more.
- **History:** persistent, respects `$HISTFILE` and `$HISTSIZE`, with private mode.

Run `swagsh --help` for the full option set.

---

## Performance

Measured with `hyperfine --shell=none` on Linux x86-64. Across builtins, variable expansion, conditionals, loops, and function calls, swagsh delivers dash-class performance: ~40% faster than bash. On pipelines, where process-spawn overhead dominates and the shell layer matters less, it's ~25% faster than bash.

---

## Installation

**Cargo:**

```sh
cargo install swagsh
```

**AUR (Arch Linux):**

```sh
paru -S swagsh   # or: yay -S swagsh
```

**From source:**

```sh
git clone https://github.com/takashialpha/swagsh.git
cd swagsh
cargo build --release   # binary at target/release/swagsh
```

---

## TODO

- `return` builtin: not yet implemented; calling it inside a function fails.
- Arithmetic expansion `$((...))` is not supported.
- Quoted heredocs (`<<'EOF'`): variable expansion inside the body is not suppressed as it should be.
- String operators: `${#var}` (length), `${var%pat}` / `${var#pat}` (trim) are not implemented.
- Reserved words (`done`, `fi`, `then`, etc.) cannot be passed as plain command arguments.
- Tab completion pager (`--More--`): prompt appears mid-output instead of at the bottom, and Ctrl-C does not exit it (only `q` works). These might be bugs in rustyline. May also try another display approach for it that solves the problem

---

## Contributing

Issues and pull requests are welcome. Please open an issue before starting work on a large change.
