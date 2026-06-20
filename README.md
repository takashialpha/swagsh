<div align="center">

# swagsh

**A sleek, high-performance Linux shell built in Rust.**
*Name inspired by* swag*, slang for stylish flair.*

[![crates.io](https://img.shields.io/crates/v/swagsh?style=flat-square&color=64b4ff&labelColor=161616)](https://crates.io/crates/swagsh)
[![AUR](https://img.shields.io/aur/version/swagsh?style=flat-square&color=64b4ff&labelColor=161616)](https://aur.archlinux.org/packages/swagsh)
[![License](https://img.shields.io/crates/l/swagsh?style=flat-square&color=64b4ff&labelColor=161616)](LICENSE)

</div>

---

## Features

- **Real shell grammar:** the POSIX constructs you reach for every day, like pipelines, redirections, substitutions, control flow, functions and subshells.
- **Full expansions:** variable, parameter, tilde and glob expansion, with sensible defaults built in.
- **Tab completion:** builtins, aliases, executables and filenames, out of the box.
- **Job control:** background jobs, foreground and background switching, stopping and signalling.
- **Configurable:** config and profile files, a customizable prompt, and persistent history.

Run `swagsh --help` for the full command set.

---

## Performance

Measured with `hyperfine --shell=none` on Linux x86-64. On pure builtins swagsh runs within about 10% of dash, the irreducible gap between Rust's startup cost and a bare C binary, and about 37% faster than bash. On fork+exec workloads the gap to bash shrinks to a few percent.

---

## Installation

**Cargo (all platforms):**

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

## Contributing

Issues and pull requests are welcome. Please open an issue before starting work on a large change.
