# Fuzzing swagsh

`cargo fuzz` targets against the pure, deterministic parts of the
interpreter: parsing, and the small string-transforming helpers behind
expansions. Everything here takes untrusted text (a script, a `$((...))`
expression, an escape sequence, a glob pattern) and the invariant that
matters is always the same: **never panic, never hang, for any input**. A
real syntax error is an `Err`/a sane fallback value, not a crash, and no
input should take more than a few milliseconds to process.

Deliberately *not* covered: anything that forks, execs, or touches
signals. That's OS-interaction code with side effects and non-determinism,
not the pure-function shape fuzzing wants, and a crash there would need a
very different harness (and a lot more caution) to investigate safely.

## Prerequisites

```sh
rustup toolchain install nightly   # cargo-fuzz needs nightly (sanitizer support)
cargo install cargo-fuzz
```

## Running

This directory is its own standalone crate (not a workspace member of the
main one; see below), so all commands run from *inside* `fuzz/`, or via
`cargo fuzz <cmd>` from the repo root, which shells out to it automatically.

```sh
# One target, until stopped (Ctrl-C) or it finds a crash:
cargo +nightly fuzz run parse

# Bound the run instead of leaving it open-ended:
cargo +nightly fuzz run parse -- -max_total_time=120

# Every target, current set:
for t in parse eval_arith unescape param_expand; do
    cargo +nightly fuzz run "$t" -- -max_total_time=60
done
```

## Targets

| Target | Exercises | Why it's a good target |
|---|---|---|
| `parse` | Parsing a script (lexing and grammar together) | Highest-value target: the whole grammar, in one call |
| `eval_arith` | Arithmetic expansion, `$(( ))` | No fallback value, so any panic is unambiguously a bug |
| `unescape` | Escape-sequence expansion (`echo -e`/`printf`) | Hand-walks bounded digit runs, the kind of manual indexing that panics on an off-by-one |
| `param_expand` | Glob/pattern matching and `${var...}` operators | Bundled into one target since none is individually large enough for its own corpus |

## On a crash or a slow-unit report

```sh
cargo +nightly fuzz tmin <target> fuzz/artifacts/<target>/<crash-or-slow-unit-file>   # minimize first
cargo +nightly fuzz run <target> fuzz/artifacts/<target>/minimized-...                # then reproduce
```

A "slow unit" (libFuzzer's timeout detector) is as much a bug as a crash
here: an algorithmically bad pattern-matcher that goes exponential on
adversarial input is a real hang, not just a theoretical one. Delete the
artifact once it's understood and fixed; they're `.gitignore`'d (along
with `target/`, `corpus/`, `coverage/`) but there's no reason to leave
them lying around locally either.

## Why this isn't a workspace member

`fuzz/`'s `Cargo.toml` depends on the main crate via a plain `path = ".."`
dependency, not `[workspace] members`. This is `cargo fuzz init`'s own
default: fuzzing needs a nightly toolchain and sanitizer-instrumented
builds the main crate has no business being built with under ordinary
`cargo build`/`cargo test`. Keeping them as two independent packages means
`cargo build`/`cargo clean`/etc. run from the repo root never touch this
directory at all.

One consequence worth knowing: **`cargo clean` at the repo root does not
free the space `fuzz/target` uses.** Run `cargo clean` from inside `fuzz/`
(or `cargo clean --manifest-path fuzz/Cargo.toml` from the root) to reclaim
that separately; it can get large (sanitizer builds are not small).

That still only clears `target/`, though: `corpus/`, `artifacts/`, and
`coverage/` are cargo-fuzz's own accumulation, not cargo's, and outlive
any `cargo clean`. `./clean.sh` clears all four in one go.
