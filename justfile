# A command runner, not a build system: cargo already knows what is stale, and
# a second layer of timestamps would only be another thing that can be wrong.
#
# Every command CI runs lives here, and CI calls these recipes rather than
# spelling the flags out again, so a green local run and a green pipeline
# cannot mean two different things. `fuzz/` needs its own recipes because it is
# a standalone crate, not a workspace member (see fuzz/README.md).

# list what there is to run
default:
    @just --list --unsorted

# build the shell; TARGET is optional (e.g. x86_64-unknown-linux-gnu)
build target="":
    #!/bin/sh
    set -eu
    # The release workflow needs an explicit target triple; a local build does
    # not. Both go through here so the profile cannot differ between them.
    if [ -n "{{ target }}" ]; then
        cargo build --release --target "{{ target }}"
    else
        cargo build --release
    fi

# everything CI runs
check: fmt lint test boundaries deps fuzz-build

# check formatting
fmt:
    cargo fmt --all -- --check

# lint with the crate's own strict lint set
lint:
    cargo clippy --all-targets -- -D warnings

# run the test suite (doctests only today)
test:
    cargo test --verbose

# check for unused dependencies
deps:
    cargo shear

# enforce the module boundaries the code relies on
boundaries:
    #!/bin/sh
    set -eu
    # `libc` is allowed in src/sys.rs and nowhere else. That module exists to
    # be the single place holding the four libc-like primitives rustix will not
    # expose publicly, so that a future backend change stays a one-file edit.
    # Nothing enforces this in the language, hence the grep.
    # `grep -v ': *//'` drops comment lines: these modules explain themselves
    # by naming the very APIs they wrap, and prose should not trip the check.
    stray=$(grep -rn 'libc::' src --include='*.rs' | grep -v ': *//' \
        | grep -v '^src/sys\.rs:' || true)
    if [ -n "$stray" ]; then
        echo "error: libc:: outside src/sys.rs:" >&2
        echo "$stray" | sed 's/^/  /' >&2
        exit 1
    fi
    # Raw file descriptors are fd.rs's business; everyone else takes the safe
    # wrappers it exports. `borrow_raw`/`from_raw_fd` outside it means an
    # unsafe fd conversion has leaked into ordinary code.
    stray=$(grep -rn 'borrow_raw\|from_raw_fd' src --include='*.rs' | grep -v ': *//' \
        | grep -vE '^src/(fd|sys)\.rs:' || true)
    if [ -n "$stray" ]; then
        echo "error: raw fd conversion outside src/fd.rs:" >&2
        echo "$stray" | sed 's/^/  /' >&2
        exit 1
    fi
    echo "boundaries ok"

# check every fuzz target still compiles against the current interpreter
fuzz-build:
    cargo build --manifest-path fuzz/Cargo.toml

# the fuzz targets cargo-fuzz can see
fuzz-list:
    @cargo fuzz list --fuzz-dir fuzz

# fuzz one target for SECONDS (cargo-fuzz needs nightly)
fuzz target seconds="60":
    cargo +nightly fuzz run {{ target }} -- -max_total_time={{ seconds }}

# fuzz every target for SECONDS each
fuzz-all seconds="60":
    #!/bin/sh
    set -eu
    # Read from cargo-fuzz rather than a list here, so a new target is picked
    # up without editing this file.
    for t in $(cargo fuzz list --fuzz-dir fuzz); do
        echo "== $t =="
        cargo +nightly fuzz run "$t" -- -max_total_time={{ seconds }}
    done

# throw away everything built, including what cargo does not own
clean:
    #!/bin/sh
    set -eu
    cargo clean
    # `fuzz/` is a separate crate, so the clean above never reaches it, and
    # cargo-fuzz's corpus/artifacts/coverage are its own accumulation rather
    # than cargo's: nothing but this removes them. See fuzz/README.md.
    cargo clean --manifest-path fuzz/Cargo.toml
    rm -rf fuzz/corpus fuzz/artifacts fuzz/coverage
    echo "removed target/, fuzz/target, fuzz/{corpus,artifacts,coverage}"
