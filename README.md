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
~ $ echo $SHELL
/usr/bin/swagsh
~ $ ls src/*.rs | sort | head -3
src/eval.rs
src/lexer.rs
src/main.rs
~ $ for f in src/*.rs; do echo "$(wc -l < $f) $f"; done | sort -rn | head -3
312 src/eval.rs
287 src/parser.rs
201 src/lexer.rs
~ $ sleep 30 &
[1] 4821
~ $ jobs
[1]+  Running    sleep 30
~ $
```

---

## Features

<table>
<tr>
<td width="50%">

🔧 **Shell grammar**

Pipelines, redirections, control flow, functions, subshells, and here-documents.

</td>
<td width="50%">

🔤 **Expansions**

Variable, parameter, tilde, glob, and command substitution.

</td>
</tr>
<tr>
<td>

⌨️ **Tab completion**

Builtins, aliases, executables, and filenames — out of the box.

</td>
<td>

⚙️ **Job control**

Background jobs, foreground and background switching, stopping, and signalling.

</td>
</tr>
<tr>
<td>

💬 **Prompt**

Customisable via `$PS1` with `\w`, `\W`, `\u`, `\h`, `\e`, `\[`/`\]`, and more.

</td>
<td>

📜 **History**

Persistent, respects `$HISTFILE` and `$HISTSIZE`, with private mode.

</td>
</tr>
</table>

---

## Performance

Measured with `hyperfine --shell=none` on Linux x86-64.

| Workload | vs bash |
|:---|:---:|
| Builtins, variable expansion, conditionals, loops, functions | **~40% faster** |
| Pipelines (process-spawn heavy) | **~25% faster** |

Dash-class performance where it counts.

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

- `return` inside functions is not yet implemented.
- Arithmetic expansion `$((...))` is not supported.
- Quoted heredocs (`<<'EOF'`) do not suppress variable expansion.
- `${#var}` (length), `${var%pat}` / `${var#pat}` (trim) are not implemented.
- Reserved words (`done`, `fi`, `then`, etc.) cannot be passed as plain arguments.
- Tab completion pager: prompt appears mid-output instead of at the bottom; `q` exits, Ctrl-C does not.

---

## Contributing

Issues and pull requests are welcome. Open an issue before starting work on a large change.
