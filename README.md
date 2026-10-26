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
~/code/swagsh on main
❯ greet() { echo "hello, ${1:-world}"; }; greet takashi
hello, takashi
~/code/swagsh on main
❯ files=(src/*.rs); echo "${#files[@]} files, the first is ${files[0]}"
23 files, the first is src/ast.rs
~/code/swagsh on main
❯ [[ $PWD == */swagsh ]] && echo "counting to $(( 2 ** 3 ))"
counting to 8
~/code/swagsh on main
❯ cargo build --release
    Finished `release` profile [optimized] target(s) in 24.31s
~/code/swagsh on main took 24s
❯ sleep 30 &
[1] 9182
~/code/swagsh on main
❯ jobs
[1]+  Running                    sleep 30 &
~/code/swagsh on main
❯ help -d cd
cd - Change the shell working directory
```

---

## Features

<table>
<tr>
<td width="50%">

🔧 **Shell language**

POSIX sh, plus bash's arrays, `[[ ]]`, `(( ))` and most of its other extensions; so all you expect.

</td>
<td width="50%">

🔤 **Expansions**

Parameter, brace, glob, arithmetic, and command and process substitution.

</td>
</tr>
<tr>
<td>

⚙️ **Job control**

Background jobs, `fg`/`bg`, signals and completion notices.

</td>
<td>

💬 **Interactive**

Syntax highlighting, Tab completion (other programs' options too, from their man pages), suggestions from history and completions, vi or emacs keys, Ctrl-X Ctrl-E to edit the line in your editor, a prompt with git and timing (or your own `$PS1`), and persistent history; all set in one [config file](#configuration).

</td>
</tr>
<tr>
<td colspan="2">

📖 **Built-in help**

`help` lists every builtin; `help NAME` (or `NAME --help`) shows its options, with examples, in color on a terminal.

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

## Compatibility notes

swagsh runs POSIX sh scripts, and most bash scripts.

### Beyond POSIX

Most of the bash extensions (`[[ ]]`, arrays, `$'...'`, `<(...)`, `local`)
use syntax that POSIX leaves unspecified or rejects, so they cannot change
what a POSIX script does. These few can:

- Brace expansion: `{a,b}` and `{1..3}` expand, as in bash, where POSIX
  keeps them as they are. Quote a brace to keep it.
- `cmd &>file` sends output and errors to `file`, and `a |& b` pipes both;
  in POSIX, `&>` is `&` followed by `>`. Write `cmd & >file` for that.
- History expansion (`!!`, `!$`) happens at an interactive prompt, inside
  double quotes too, as in bash; `set +H` turns it off. Never in scripts.
- At an interactive prompt, `fc` without `-e` or `$FCEDIT` opens the
  config file's `editor.command`, else `$VISUAL`, `$EDITOR` or `vi`, much
  as bash does. POSIX says `ed`, which scripts still get.

### Where POSIX leaves the choice

- `echo` is bash's: it takes `-n`, `-e` and `-E`, and expands backslash
  escapes only with `-e`. Use `printf` for portable output.
- Every command of a pipeline runs in a subshell, the last one too, unless
  `shopt -s lastpipe`.
- Arithmetic is on 64-bit signed integers, which wrap around on overflow.
  Division by zero is an error.
- `local`, and `declare` in a function, scope variables dynamically, as in
  bash: a function sees the variables of the functions that called it.
- `~user` for an unknown user stays as written.
- In a pattern, `[^...]` is `[!...]`.
- An expansion error (`${x?}`, an unset variable under `set -u`) ends a
  non-interactive shell; under `set -u`, with status 127, or 1 with `set -e`,
  as bash does.
- A script that does not exist exits with 127; one that cannot be read,
  with 126.
- A here-document that runs into the end of input ends there, with a
  warning.
- A `'` inside a double-quoted `${...}`, as in `"${x:-it's}"`, is an
  ordinary character, as in dash.
- A function name can have characters beyond a POSIX name (`a-b`, `a.b`),
  as in bash, but no quoting or expansions.
- Aliases expand in scripts too, as POSIX says. bash expands them only in
  its POSIX mode or with `shopt -s expand_aliases`.
- Commands read from a pipe on standard input are read a byte at a time,
  so that the commands they run can read the rest of it.
- `set`, `export -p`, `readonly -p`, `alias` and `trap` print text that
  reads back as the same settings. `set -o`, `shopt` and `kill -l` print
  bash's layout.

### Where it differs from bash, on purpose

- `.*` never matches `.` or `..` (as in bash 5.2 and later).
- Associative arrays list their keys in sorted order.
- `BASH_VERSION` is unset; scripts can check `SWAGSH_VERSION` instead.
- `shopt` takes the options `shopt` lists, and rejects the rest by name.
- `$-` lists only the options swagsh has, so never bash's `h` or `B`.
- `printf '%a'` prints the same number in a different hexadecimal form:
  `0x1p+0` for 1, as C's `printf` does, where bash prints `0x8p-3`.
- `declare -f` and `type` print functions in swagsh's own style (`${name}`
  for `$name`, single quotes where nothing expands). It reads back as the
  same function.

---

## Configuration

An interactive shell reads `~/.config/swagsh/config.toml`
(`$XDG_CONFIG_HOME/swagsh/config.toml` if that is set). The first one to
start writes it, with every setting at its default and a comment on what
each does. Any section or key can be deleted to go back to its default. A
mistake in the file is reported at startup, and the defaults are used
instead. Scripts never read it.

```toml
[banner]
enabled = true       # the logo and version at startup
tips = true          # and a tip under them

[prompt]
keys = "emacs"       # how the line is edited: "emacs" or "vi"; `set -o vi`
                     # and `set -o emacs` switch too
format = ""          # the prompt, written as $PS1 is, with \g for git and \c
                     # for how long the last command took; empty for
                     # swagsh's own. $PS1 comes first when it is set
newline = true       # swagsh's own prompt: the symbol on a line of its own
symbol = "❯"         # what swagsh's own prompt ends in
directory = "bold cyan"
success = "bold green"  # the symbol after a command that worked
failure = "bold red"    # and after one that failed

[prompt.git]
enabled = true       # the branch, inside a repository
status = true        # and its state: * changed, + staged, ? untracked,
                     # ↑ ahead, ↓ behind; this runs `git status` each prompt
color = "magenta"

[prompt.duration]
enabled = true       # how long the last command took
min = 2.0            # if it took at least this many seconds
color = "yellow"

[editor]
command = ""         # the program Ctrl-X Ctrl-E (`v` with vi keys) and fc
                     # open, such as "nvim" or "code --wait"; empty for
                     # $VISUAL, then $EDITOR, then vi

[completion]
case = "smart"       # ignore case unless the word has a capital; or
                     # "sensitive", or "insensitive"
man = true           # complete other programs' options from their man pages

[suggestions]        # the rest of the line, in grey, as you type; → takes it
enabled = true
history = true       # from commands typed before, if what they name still exists
completions = true   # else the rest of a word with only one completion

[highlight]
enabled = true
command = "green"    # a command that exists
unknown = "red"      # one that does not
keyword = "magenta"
string = "yellow"
variable = "cyan"
operator = "bold"
option = "blue"
comment = "dim"
path = "underline"   # an argument naming a file that exists
error = "bold red"   # an unclosed quote, and the like
bracket = "bold reverse"
suggestion = "dim"

[history]
size = 1000          # $HISTSIZE overrides it
ignore_dups = true
ignore_space = true  # a command starting with a space is not kept
```

A color is any mix of names (`red`, `bright-blue`), attributes (`bold`,
`dim`, `italic`, `underline`, `reverse`), a 256-color number, and
`#rrggbb`, separated by spaces.

The prompt is the first of these that is set:

1. `$PS1`. swagsh never sets it, so it is only set when you set it, in
   `~/.swagshrc` for example, or when it comes from the environment.
   `unset PS1` goes back to the next one.
2. `prompt.format` in this file.
3. swagsh's own: the directory, the git branch and its state, and how long
   the last command took when that was long; then, on the line under them,
   `❯` in green, or in red after a command that failed. The `[prompt]`
   keys and sections style it.

   ```
   ~/code/swagsh on main *?↑2 took 1m27s
   ❯
   ```

`$PS1` and `prompt.format` take the same escapes as bash's `$PS1` (`\w`,
`\u`, `\h`, `\$`, ...), plus two of swagsh's own. Each of the two has a
space before it, and is empty when there is nothing to show:

- `\g`: the git branch and its state, as `[prompt.git]` says.
- `\c`: how long the last command took, as `[prompt.duration]` says.

So `format = "\\w\\g\\c \\$ "` gives `~/code/swagsh main * 3.2s $` (TOML
doubles the backslashes). The branch is read from `.git` directly, so
`status = false` keeps the prompt instant in a very large repository.

Startup files are still sourced as before: `~/.swagsh_profile` for a login
shell, then `$ENV` and `~/.swagshrc` for an interactive one.

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

### Interactive features stay interactive

Highlighting, completion, the banner and the configuration file must never
change how a script runs, or how fast. They live in `src/repl/` and
`src/prompt.rs`, which only an interactive shell reaches (apart from
sourcing the startup files). Outside them, swagsh's own output (errors, job
states, `set -o` and `shopt` listings, help pages) is colored only through
`src/paint.rs` and the help page styles, and only when it goes to a
terminal with `$NO_COLOR` unset, so what a script pipes or captures stays
plain text. Nothing enforces this at the type level, so keep it in mind
when touching those modules.

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
