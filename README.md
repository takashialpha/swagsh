# swagsh

**A sleek, high-performance Linux shell built in Rust.**  
Name inspired by *swag* slang for stylish flair.

---

[![crates.io](https://img.shields.io/crates/v/swagsh?style=flat-square&color=64b4ff&labelColor=161616)](https://crates.io/crates/swagsh)
[![AUR](https://img.shields.io/aur/version/swagsh?style=flat-square&color=64b4ff&labelColor=161616)](https://aur.archlinux.org/packages/swagsh)
[![License](https://img.shields.io/crates/l/swagsh?style=flat-square&color=64b4ff&labelColor=161616)](LICENSE)

---

## Performance

Measured with `hyperfine --warmup 50 --shell=none` on Linux x86-64 (CachyOS). Times are wall-clock means; ratio is relative to swagsh — **lower is faster**.

| Workload | swagsh | dash | bash |
|---|---|---|---|
| `exit` (startup floor) | 1.00× (627 µs) | **0.90×** (563 µs) | 1.38× (867 µs) |
| `:` (no-op builtin) | 1.00× (630 µs) | **0.90×** (564 µs) | 1.37× (861 µs) |
| `echo hello` | 1.00× (631 µs) | **0.90×** (570 µs) | 1.38× (871 µs) |
| `x=1; echo $x` | 1.00× (642 µs) | **0.89×** (572 µs) | 1.37× (879 µs) |
| `/bin/true` (fork+exec) | 1.00× (1081 µs) | **0.78×** (844 µs) | 1.04× (1123 µs) |

**Summary:** swagsh is ~10% slower than dash on pure builtins (the irreducible gap between Rust's startup cost and a bare C binary), and ~37% faster than bash. On fork+exec workloads the gap to bash shrinks to ~4%.

---

## Installation

**From source:**

```sh
git clone https://github.com/takashialpha/swagsh.git
cd swagsh
cargo build --release
```

Binary is at `target/release/swagsh`. Optionally install system-wide:

```sh
sudo cp target/release/swagsh /usr/local/bin/swagsh
```

**From crates.io:**

```sh
cargo install swagsh
```

**AUR (Arch Linux):**

```sh
paru -S swagsh
# or
yay -S swagsh
```

> **Warning:** Do not replace `/bin/sh` without thorough testing. The project is under active development.

---

## Usage

```sh
# Interactive REPL
swagsh

# Execute a command string
swagsh -c "echo hello"

# Run a script
swagsh script.sh arg1 arg2

# Syntax check only (no execution)
swagsh -n script.sh

# Private mode (no history read or written)
swagsh -P

# Skip config files (~/.swagshrc, ~/.swagsh_profile)
swagsh -N
```

---

## Configuration

| File | When sourced |
|---|---|
| `~/.swagshrc` | Every interactive session |
| `~/.swagsh_profile` | Login shells only |

History is saved to `~/.swagsh_history` (override with `$HISTFILE`).

**Prompt customization** via `$PS1` — supported escape sequences:

| Sequence | Expands to |
|---|---|
| `\w` | Current working directory (`$HOME` → `~`) |
| `\u` | Username (`$USER`) |
| `\h` | Hostname (short) |
| `\$` | `#` for root, `$` otherwise |
| `\n` | Newline |

---

## Features

### Builtins

`:`, `.`, `[`, `[[`, `alias`, `bg`, `break`, `cd`, `continue`, `echo`, `exec`, `exit`, `export`, `false`, `fg`, `jobs`, `kill`, `printf`, `pwd`, `read`, `set`, `shift`, `source`, `test`, `true`, `unalias`, `unset`

### Shell constructs

- Pipelines: `cmd1 | cmd2 | cmd3`
- And-or lists: `cmd1 && cmd2`, `cmd1 || cmd2`
- Background jobs: `cmd &`
- Redirection: `>`, `>>`, `<`, `<<`, `>&`, `&>`, here-strings (`<<<`)
- Command substitution: `$(cmd)`, `` `cmd` ``
- Variable expansion: `$VAR`, `${VAR:-default}`, `${VAR:+alt}`, `${VAR:?err}`, `${VAR:=default}`
- Tilde expansion, glob expansion (`*`, `?`)
- Control flow: `if`/`elif`/`else`, `for`, `while`, `until`, `case`
- Functions, command groups `{ }`, subshells `( )`
- Aliases with tab completion
- Tab completion: builtins, aliases, `$PATH` executables, filenames

### Job control

`fg`, `bg`, `jobs`, `kill`, Ctrl+Z (stop), `&` (background)

---

## Known limitations

- No multiline REPL (`PS2` prompt)
- No `local`, `return`, `eval`, `trap`
- No arithmetic expansion (`$((...))`)
- No `[...]` glob character class matching
- No `[[ =~ ]]` regex matching
- No `set -e`/`-u`/`-x`
- `$@` / `"$@"` quoting semantics are incomplete
- `&>>` is not supported (parsed but treated as `>>`)
- No `wait`, `disown`, full `SIGCHLD` handling
- Heredoc expansion is partial

---

## Wiki

See the [wiki](https://github.com/takashialpha/swagsh/wiki) for extended documentation:
- [Usage](https://github.com/takashialpha/swagsh/wiki/Usage) — full command reference
- [Performance](https://github.com/takashialpha/swagsh/wiki/Performance) — benchmark methodology and raw data
