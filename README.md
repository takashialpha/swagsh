# swagsh

**A sleek, high-performance POSIX-compatible shell built in Rust for speed and reliability.**  
Name inspired by *swag* slang for stylish flair.

---

## Features

- **POSIX + bash-ish** — pipelines, redirections, heredocs, `if`/`for`/`while`/`until`/`case`, functions, subshells, brace groups
- **Builtins** — `cd`, `echo`, `printf`, `export`, `unset`, `set`, `source`/`.`, `exec`, `exit`, `read`, `shift`, `test`/`[`/`[[`, `alias`/`unalias`, `jobs`, `fg`, `bg`, `kill`, `true`, `false`, `:`
- **Job control** — foreground/background execution, `Ctrl-Z` to stop, `fg`/`bg`/`kill`
- **Tab completion** — builtins, aliases, and PATH executables on the first word; file completion on arguments
- **History** — persistent across sessions via `~/.swagsh_history`; `--private` mode disables read/write
- **Aliases** — `alias`/`unalias`, expanded in all execution contexts including pipeline stages
- **Variable expansion** — `$VAR`, `${VAR}`, `${VAR:-default}`, `${VAR:=val}`, `${VAR:+alt}`, `${VAR:?err}`, `$?`, `$$`, `$@`, `$*`, `$#`, `$0`–`$9`
- **Tilde expansion** — `~` and `~/path` everywhere, not just in `cd`
- **Glob expansion** — `*`, `?`, `[...]` with full path prefix support (`src/*.rs`)
- **Heredocs** — `<<EOF` and `<<-EOF` with variable expansion in the body
- **Here-strings** — `<<<word`
- **POSIX §2.9.1 assignments** — `x=1`, `FOO=bar cmd`, temporary exports before commands
- **`test`/`[`/`[[`** — file tests, string tests, integer tests, logical operators, `&&`/`||`/`!` in `[[`

---

## Performance

swagsh is within **~16% of dash** on all builtin operations — the irreducible gap is Rust runtime startup vs a bare C binary. It is **~40% faster than bash** on equivalent workloads.

| Command | swagsh | dash | bash |
|---|---|---|---|
| `exit` (startup floor) | 1.00× | **0.86×** | 1.44× |
| `:` (no-op builtin) | 1.00× | **0.86×** | 1.42× |
| `echo hello` | 1.00× | **0.85×** | 1.44× |
| `x=1; echo $x` | 1.00× | **0.83×** | — |
| `/bin/true` (fork+exec) | 1.00× | **0.51×** | — |

*All times relative to swagsh. Lower is faster. Measured with `hyperfine --warmup 50 --shell=none` on Linux x86-64.*

Full benchmark details: [wiki/Performance](https://github.com/takashialpha/swagsh/wiki/Performance)

---

## Installation

```bash
git clone https://github.com/takashialpha/swagsh.git
cd swagsh
cargo build --release
```

Binary at `target/release/swagsh`. Optionally install system-wide:

```bash
sudo cp target/release/swagsh /usr/local/bin/swagsh
```

> ⚠️ Do not replace `/bin/sh` without thorough testing. The project is under active development and bugs may exist.

---

## Usage

```bash
# Interactive
swagsh

# Execute a command string
swagsh -c "echo hello"

# Run a script
swagsh script.sh arg1 arg2

# Syntax check only
swagsh -n script.sh

# Private mode (no history)
swagsh -P

# Skip config file
swagsh -N
```

Full usage reference: [wiki/Usage](https://github.com/takashialpha/swagsh/wiki/Usage)

---

## Wiki

See the [wiki](https://github.com/takashialpha/swagsh/wiki) for:
- [Usage](https://github.com/takashialpha/swagsh/wiki/Usage) — full command reference and examples
- [Performance](https://github.com/takashialpha/swagsh/wiki/Performance) — detailed benchmarks vs dash and bash

---

## License

Apache-2.0 — see [LICENSE](LICENSE).
