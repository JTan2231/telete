#!/bin/sh
set -eu

repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)
cd "$repo"
# Private-path checks reject symbolic ancestors, including macOS's /var alias.
telete_tmp=${TMPDIR:-/tmp}
TMPDIR=$(CDPATH= cd -- "$telete_tmp" && pwd -P)
export TMPDIR
cargo fmt --all --check
cargo clippy --locked -p telete --all-targets -- -D warnings -D clippy::all -D clippy::pedantic
# The inherited-lock fixtures must not share a process with concurrent tests.
cargo test --locked -p telete -- --test-threads=1
cargo build --locked --bin telete
