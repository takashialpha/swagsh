# swagsh TODO

## In Progress

### High Priority (correctness)
- [ ] `local` builtin — function-scoped variables
- [ ] `PS2` prompt — continuation prompt for multi-line input in REPL
- [ ] `$( )` in assignment RHS — `x=$(date)` currently broken
- [ ] Multi-line input continuation — `if true;` + Enter should show `> ` and wait
- [ ] `eval` builtin — dynamic code execution
- [ ] `trap` builtin — signal handling in scripts

### Medium Priority
- [ ] `shift` with argument — `shift N` (shift by N, currently shift 1 only... verify)
- [ ] `printf %b` format specifier — escape sequences
- [ ] Recursive alias expansion — `alias ls='ls --color'` without infinite loop
- [ ] `return` builtin — exit from function with status code
- [ ] `type` builtin — show if name is builtin/function/alias/external
- [ ] `which`-like behavior — resolve command to full path
- [ ] `$( )` inside `[[ ]]` — command substitution in test expressions
- [ ] `=~` operator in `[[ ]]` — regex matching

### Low Priority / Polish
- [ ] `~username` expansion — expand to another user's home directory
- [ ] `${#VAR}` — string length expansion
- [ ] `${VAR#pattern}` / `${VAR%pattern}` — prefix/suffix stripping
- [ ] `${VAR/pat/rep}` — substring replacement
- [ ] Heredoc with quoted delimiter — `<<'EOF'` should suppress expansion (verify)
- [ ] `<<<` here-string variable expansion (verify correct)
- [ ] `set -e` — exit on error
- [ ] `set -x` — trace execution
- [ ] `set -u` — error on unset variables
- [ ] `set -o pipefail` — pipeline exit status
- [ ] `2>&1` fd duplication in redirections (verify)
- [ ] `/dev/stdin`, `/dev/stdout`, `/dev/stderr` as redirect targets
- [ ] `export -n` — unexport a variable
- [ ] `readonly` builtin
- [ ] `declare` builtin (bash compat)
- [ ] `mapfile`/`readarray` builtin

## Performance
- [ ] PATH lookup caching — hash table of resolved binary paths (like bash's `hash`)
- [ ] Reduce `fork()` calls for simple pipelines where possible
- [ ] Pre-compute `$PS1` only when cwd/user changes, not every prompt

## Config & UX
- [ ] Source `~/.swagshrc` on startup (respects `--no-config`)
- [ ] Source `/etc/swagsh/swagshrc` system config
- [ ] `$HISTSIZE` — configurable history size
- [ ] `$HISTFILE` — already done, verify on startup
- [ ] History deduplication — don't add duplicate consecutive entries
- [ ] History search — Ctrl-R (rustyline may handle this already)
- [ ] `CDPATH` — search path for `cd`
- [ ] `$PROMPT_COMMAND` — command run before each prompt
- [ ] Multiline history — store/restore multi-line commands as single entries
- [ ] Login shell — source `~/.profile` when `argv[0]` starts with `-`
- [ ] `--no-config` flag — already in CLI, wire to skip rc sourcing

## Job Control
- [ ] Print job completion notice on next prompt (not mid-output)
- [ ] `jobs -l` — show PIDs in job listing
- [ ] `jobs -p` — show only PIDs
- [ ] `disown` builtin — remove job from table without killing it
- [ ] `wait` builtin — wait for background job by id or pid
- [ ] Proper `SIGCHLD` handler — async reaping instead of polling

## Correctness / POSIX Compliance
- [ ] Word splitting respects quoted regions — `"$x"` should not split even if x contains IFS
- [ ] Glob in double quotes should not expand — `"*.rs"` should be literal
- [ ] `$@` in double quotes — `"$@"` should expand to separate quoted words
- [ ] `$*` vs `$@` semantics — currently both join with space
- [ ] Exit status of last background job via `$!`
- [ ] `LINENO` special variable
- [ ] `SECONDS` special variable
- [ ] Process substitution `<(cmd)` and `>(cmd)` — bash extension
- [ ] Arithmetic expansion `$(( ))` — currently errors; implement basic integer math
- [ ] Compound assignment `((x++))` — bash extension
- [ ] `break N` / `continue N` — break/continue N levels of nesting
- [ ] `case` fallthrough with `;&` and `;;&`
- [ ] Function `local` scope for positional params during recursion
- [ ] Subshell exit status propagation in `$( )`
- [ ] Correct `IFS` handling — multi-char IFS, IFS with special chars

## Known Bugs
- [ ] Paste-triggering heredoc `> ` prompts when `#` comment contains `<<`  
      (partial fix: skip `#` in `extract_heredoc_delimiters`, but multi-line paste still triggers)
- [ ] `[[ ]]` inside pipelines — `[[ test ]] | cat` may not work correctly
- [ ] Alias expansion is non-recursive — `alias ls='ls --color'` causes infinite loop risk (verify it doesn't loop)
- [ ] `glob_expand` doesn't handle `**` recursive glob
- [ ] `expand_heredoc_body` `$(cmd)` expansion creates sub-executor — expensive
- [ ] `test`/`[` `-a`/`-o` deprecated in POSIX; `[[` `&&`/`||` preferred
- [ ] Tab completion doesn't complete variable names (`$HO` → `$HOME`)
- [ ] Tab completion doesn't handle `~user` expansion
