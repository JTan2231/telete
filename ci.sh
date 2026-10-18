#!/bin/sh
set -eu

repo=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
binary=${TELETE_BIN:-"$repo/target/debug/telete"}
if [ ! -x "$binary" ]; then
    printf '%s\n' 'Build Telete with cargo build --locked --bin telete, or set TELETE_BIN.' >&2
    exit 1
fi
if [ "$#" -eq 0 ]; then
    exec "$binary" --help
fi
exec "$binary" --config "$repo/ci/workflow.json" "$@"
