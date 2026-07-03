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
cargo build --release   # binary at target/release/swagsh
```

---

## Known limitations

- A redirect trailing a compound command (`while ... done < file`,
  `{ ...; } > file`, same for `if`/`for`/`case`/`( )`) is silently wrong,
  not just unsupported: it parses without error but has no effect on the
  command, since the AST has nowhere to attach it (only a plain simple
  command carries redirects today). `while read line; do ...; done < file`
  runs with whatever stdin the shell already had, never touching `file`, a
  real hazard given how common that exact idiom is. Fixing it properly
  needs a redirect list on every compound-command AST node plus parser and
  evaluator support to match, not a small patch.
- No shell arrays (`arr=(a b c)`, `${arr[@]}`); a language-level gap, not a
  missing builtin, but it's why `declare`/`mapfile`/`readarray` aren't in
  the list below either: they'd need this first.

### Missing builtins:

  - `local`: functions currently share the caller's variable scope entirely.
  - `trap`: no signal or exit-handler registration.
  - `umask`, `readonly`: straightforward, just not done yet.
  - `hash`: PATH-lookup cache (`hash`/`hash -r`); still genuinely useful, not
    legacy, worth doing alongside `command`/`type`.
  - `builtin`: forces NAME to resolve to an actual builtin, erroring instead
    of falling through to PATH if it isn't one (`command` already bypasses a
    same-named function to reach a builtin, so `builtin` is a narrower,
    stricter tool on top of that, not a replacement for it; mainly useful
    inside a function that shadows a builtin's name and needs to call the
    real one, e.g. a `cd` wrapper calling `builtin cd "$@"`).
  - `help`: planned. Every builtin will implement a shared `Help` trait, with
    a default impl derived from its existing `clap` command so most builtins
    get this for free; the hand-written non-`clap` builtins (`:`, `true`,
    `false`, `[`, `test`) and `help` itself provide their own impl.
  - `ulimit`: resource limits.
  - `times`: POSIX-specified cumulative shell/children CPU time; just not
    done yet. Distinct from `time` below (which times one pipeline).
  - `fc`: POSIX-specified, but its re-edit-and-rerun-from-history workflow is
    already covered by line-editing and arrow-key recall; low priority.
- Not POSIX, but common enough to be worth adding later: `pushd`/`popd`/`dirs`
  (directory stack), `declare`/`typeset` (blocked on array support above),
  `disown`, `let` (thin wrapper around the arithmetic evaluator `$(( ))`
  already uses internally), `history` (list/clear persistent history; today
  history is file-backed only, with no builtin to inspect or manage it from
  the shell itself), `time` (times a single pipeline; ubiquitous in
  interactive use despite not being POSIX-specified as a builtin).

---

## Contributing

Issues and pull requests are welcome. Open an issue before starting work on a large change.

The parser and other pure interpreter internals are fuzz-tested; see
[`fuzz/README.md`](fuzz/README.md) for targets and how to run them.
