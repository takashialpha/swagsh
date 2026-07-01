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

- Reserved words (`done`, `fi`, `then`, etc.) cannot be passed as plain arguments.
- No `local` builtin; functions share the caller's variable scope.
- No `trap` builtin.

---

## Contributing

Issues and pull requests are welcome. Open an issue before starting work on a large change.
