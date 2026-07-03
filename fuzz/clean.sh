#!/bin/sh
# Frees everything `cargo clean` doesn't: cargo only owns `target/` here,
# but cargo-fuzz also accumulates `corpus/`, `artifacts/`, and `coverage/`
# on its own, growing across fuzzing runs. See fuzz/README.md for why
# `cargo clean` at the repo root never reaches any of this in the first
# place (this crate isn't a workspace member).
set -e

cd -- "$(dirname -- "$0")"

cargo clean
rm -rf corpus artifacts coverage

echo "fuzz/: target, corpus, artifacts, coverage removed"
