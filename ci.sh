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
command=$1
shift
case "$command" in
    init)
        exec "$binary" --state "$repo/.telete" init --repo "$repo" "$@"
        ;;
    submit)
        exec "$binary" --state "$repo/.telete" submit "$@" --repo "$repo" \
            --workflow "$repo/ci/workflow.json"
        ;;
    *)
        exec "$binary" --state "$repo/.telete" "$command" "$@"
        ;;
esac
