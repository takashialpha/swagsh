<div align="center">

<pre>
                         ▗▖   
                         ▐▌   
▗▟██▖█   █ ▟██▖ ▟█▟▌▗▟██▖▐▙██▖
▐▙▄▖▘▜ █ ▛ ▘▄▟▌▐▛ ▜▌▐▙▄▖▘▐▛ ▐▌
 ▀▀█▖▐▙█▟▌▗█▀▜▌▐▌ ▐▌ ▀▀█▖▐▌ ▐▌
▐▄▄▟▌▝█ █▘▐▙▄█▌▝█▄█▌▐▄▄▟▌▐▌ ▐▌
 ▀▀▀  ▀ ▀  ▀▀▝▘ ▞▀▐▌ ▀▀▀ ▝▘ ▝▘
                ▜█▛▘          
</pre>

**A fast, minimal, modern Linux shell. Named after swag, slang for stylish flair.**

[![crates.io](https://img.shields.io/crates/v/swagsh?style=flat-square&color=64b4ff&labelColor=161616)](https://crates.io/crates/swagsh)
[![AUR](https://img.shields.io/aur/version/swagsh?style=flat-square&color=64b4ff&labelColor=161616)](https://aur.archlinux.org/packages/swagsh)
[![License](https://img.shields.io/crates/l/swagsh?style=flat-square&color=64b4ff&labelColor=161616)](LICENSE)

</div>

---

```sh
$ swagsh
~ ❯ greet() { echo "hello, ${1:-world}"; }
~ ❯ greet takashi
hello, takashi
~ ❯ count=$((1 + 2 + 3)); echo $count
6
~ ❯ for f in /etc/os-release /etc/hostname; do
>   [ -f "$f" ] && echo "$f: $(head -1 $f)"
> done
/etc/os-release: NAME="Arch Linux"
/etc/hostname: mymachine
~ ❯ sleep 5 &
[1] 9182
~ ❯ jobs
[1]  Running    sleep 5
~ ❯
```

---

## Features

<table>
<tr>
<td width="50%">

🔧 **Shell grammar**

The constructs you expect: pipelines, redirections, control flow, functions, subshells, and here-documents.

</td>
<td width="50%">

🔤 **Expansions**

Variable, tilde, glob, command substitution, arithmetic, and parameter operators.

</td>
</tr>
<tr>
<td>

⚙️ **Job control**

Background jobs, foreground and background switching, stopping, and signalling.

</td>
<td>

💬 **Prompt and history**

Customisable `$PS1`, persistent history with `$HISTFILE`/`$HISTSIZE`.

</td>
</tr>
</table>

---

## Performance

Measured with `hyperfine --shell=none` on Linux x86-64, across a battery of
workloads: interpretation-heavy loops, function calls and recursion,
parameter expansion, `case`/glob pattern matching, real filesystem
globbing, and process-spawn-heavy pipelines and command substitution.

| Workload | vs bash |
|:---|:---:|
| Recursive function calls | **~6.6x faster** |
| Parameter expansion (`${var...}`) | **~3.6x faster** |
| Function calls | **~3.2x faster** |
| Arithmetic/conditional loops | **~2.7x faster** |
| `case`/glob pattern matching | **~2.5x faster** |
| Real filesystem globbing | **~2.1x faster** |
| Startup | **~1.4x faster** |
| Pipelines, command substitution (process-spawn heavy) | **~1.2-1.4x faster** |

`dash` is the harder bar: hand-tuned C, no interpreter overhead to shave.
swagsh wins outright there too on every interpretation-heavy workload
above (loops, function calls, pattern matching, glob expansion,
recursion), and sits within single-digit percent on the rest. The two
categories where `dash` still comes out ahead (parsing a very large script,
and a workload built around thousands of distinct variables) are real,
modest, and understood: not a mystery to chase, just fork/exec and
memory-layout mechanics that are hard to beat a decades-tuned C shell at.
Against `busybox`'s `ash` (a statically-linked multi-call binary, structurally
close to the fastest possible fork/exec cost) swagsh still wins the
interpretation-heavy majority, trailing only on the narrow set of
workloads that are almost pure process-spawn cost rather than
interpretation.

---

## Installation

**Cargo**

```sh
cargo install swagsh
```

**AUR (Arch Linux)**

```sh
paru -S swagsh   # or: yay -S swagsh
```

**From source**

```sh
git clone https://github.com/takashialpha/swagsh.git
cd swagsh
just build   # binary at target/release/swagsh
```

`just build` is a thin wrapper over `cargo build --release`, the same recipe the release workflow uses; plain cargo works too if you do not have [just](https://github.com/casey/just). Run `just` to list the other recipes.

Build with the latest stable Rust: swagsh tracks the stable channel rather than supporting a minimum version, and uses new language features as they land. If cargo reports your toolchain is too old, run `rustup update stable`.

---

## Known limitations

- No shell arrays (`arr=(a b c)`, `${arr[@]}`); a language-level gap, not a
  missing builtin, and the reason `declare`/`typeset`/`mapfile`/`readarray`
  are absent too: they would need this first.
- `set -b` (report background jobs the moment they finish) is not
  implemented, and is rejected rather than accepted and ignored. Jobs are
  reported before the next prompt instead.
- `trap` lets a non-interactive shell trap a signal that was ignored when it
  started, which POSIX forbids, and a trapped signal does not cut a `wait`
  short. Real-time signals can be listed but not trapped or sent.
- Pathname expansion sorts by byte value, as in the C locale, rather than by
  the locale's collation order, and `.*` does not match `.` or `..` (as in
  bash 5.2 and later).
- The interactive prompt asks for a here-document's body only when `<<`
  appears in the typed line itself, not when it comes from an alias.

### Missing builtins:

  - `help`: planned. Every builtin will implement a shared `Help` trait, with
    a default impl derived from its existing `clap` command so most builtins
    get this for free; the hand-written non-`clap` builtins (`:`, `true`,
    `false`, `[`, `test`) and `help` itself provide their own impl.
  - `declare`/`typeset`, blocked on array support above.

---

## TODO

### Planned interactive work

All of this has to stay strictly on the interactive side: none of it may
change script execution or its performance. That separation holds today by
construction, since `repl.rs` and `prompt.rs` are reachable only from
`main`'s interactive branch and the only ANSI escapes outside `prompt.rs`
are `echo -e`'s own, but nothing enforces it at the type level.

- A startup banner when launched interactively.
- Completion and syntax highlighting, through rustyline's `Completer` and
  `Highlighter`.
- A configuration file, beyond the `~/.swagshrc`, `~/.swagsh_profile` and
  `$ENV` script sourcing that already exists.

---

## Contributing

Issues and pull requests are welcome. Open an issue before starting work on a large change.

Every command CI runs is a [`just`](https://github.com/casey/just) recipe, and
the workflows call those recipes rather than repeating the flags, so a green
local run and a green pipeline cannot mean two different things. `just` on its
own lists what there is; `just check` is the whole gate:

```sh
just check        # fmt + lint + test + boundaries + deps + fuzz-build
just fuzz parse   # fuzz one target (needs nightly and cargo-fuzz)
just clean        # target/, and the corpus/artifacts cargo does not own
```

### Two modules own the unsafe

The syscall surface is deliberately confined, and `just boundaries` enforces it
because Rust has no way to express it as a lint:

- **`src/sys.rs` is the only place allowed to name `libc::`.** It holds the
  four libc-like primitives rustix does not expose publicly (`fork`, `execve`,
  `sigaction`, `sigprocmask`). rustix hid its `runtime` module behind a name it
  says "will rotate periodically", so depending on that meant breaking on every
  rustix release; keeping the FFI in one file is what made moving off it a
  one-file change.
- **`src/fd.rs` owns every raw-descriptor conversion.** No `borrow_raw` or
  `from_raw_fd` outside it. Callers take its safe wrappers instead.

Everything else goes through rustix's stable public API, which is worth using
directly: it is the experimental `runtime` corner that moved, not the rest.

The parser and other pure interpreter internals are fuzz-tested; see
[`fuzz/README.md`](fuzz/README.md) for targets and how to run them.
