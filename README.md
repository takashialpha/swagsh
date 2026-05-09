# swagsh

**A sleek, high-performance Linux shell built in Rust for speed and reliability.**  
Name inspired by *swag* slang for stylish flair.

---

[![crates.io](https://img.shields.io/crates/v/swagsh?style=flat-square&color=64b4ff&labelColor=161616)](https://crates.io/crates/swagsh)
[![AUR](https://img.shields.io/aur/version/swagsh?style=flat-square&color=64b4ff&labelColor=161616)](https://aur.archlinux.org/packages/swagsh)
[![License](https://img.shields.io/crates/l/audium?style=flat-square&color=64b4ff&labelColor=161616)](LICENSE)
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

## TODO

- [ ] Multiline REPL (`PS2`)
- [ ] `local`, `return`, `eval`, `trap`
- [ ] Fix `$(...)` + shell expansions
- [ ] Proper quoting / `$@` semantics
- [ ] Arithmetic + parameter expansion
- [ ] `[[ =~ ]]`, `set -e/-u/-x`
- [ ] Job control (`wait`, `disown`, `SIGCHLD`)
- [ ] rc/profile sourcing + history UX
- [ ] PATH cache + fewer `fork()`
- [ ] Fix heredoc / alias / pipeline quirks
- [ ] Deeper testing to see what's missing
- [ ] Optimize it for even better performance
- [ ] check Code/Syntax correctness further than clippy
- [ ] Improve UX/UI

## Wiki

See the [wiki](https://github.com/takashialpha/swagsh/wiki) for:
- [Usage](https://github.com/takashialpha/swagsh/wiki/Usage) — full command reference and examples
- [Performance](https://github.com/takashialpha/swagsh/wiki/Performance) — detailed benchmarks vs dash and bash
