#!/bin/sh
# The candidate owns this entry point and the compiled Cell validator.
# The queue manager supplies the exact validator and frozen validation arguments.
set -eu

if [ "$#" -eq 0 ]; then
    echo 'cell check: expected the candidate validator executable' >&2
    exit 2
fi

validator=$1
shift
exec "$validator" "$@"
