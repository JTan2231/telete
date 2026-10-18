#!/bin/sh
set -eu

if [ "$#" -gt 1 ]; then
    printf '%s\n' 'Usage: ci/bump-version.sh [patch|minor|major]' >&2
    exit 2
fi
bump=${1:-patch}
case "$bump" in
    patch|minor|major) ;;
    *)
        printf '%s\n' 'Usage: ci/bump-version.sh [patch|minor|major]' >&2
        exit 2
        ;;
esac

repo=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd -P)
staging=$(mktemp -d "$repo/.version.XXXXXX")
trap 'rm -rf -- "$staging"' EXIT
trap 'exit 1' HUP INT TERM

# Validate all three version records before replacing any source files.
awk -v bump="$bump" -v staging="$staging" '
    function fail(message) {
        print "bump-version: " message > "/dev/stderr"
        failed = 1
        exit 1
    }
    FILENAME == ARGV[1] {
        if ($0 ~ /^\[/) package = ($0 == "[package]")
        if (package && $0 ~ /^version = "/) {
            split($0, fields, /"/)
            old = fields[2]
            if (old !~ /^(0|[1-9][0-9]*)[.](0|[1-9][0-9]*)[.](0|[1-9][0-9]*)$/)
                fail("expected a major.minor.patch package version")
            split(old, parts, /[.]/)
            if (bump == "major") {
                parts[1]++; parts[2] = 0; parts[3] = 0
            } else if (bump == "minor") {
                parts[2]++; parts[3] = 0
            } else parts[3]++
            version = sprintf("%.0f.%.0f.%.0f", parts[1], parts[2], parts[3])
            $0 = "version = \"" version "\""
            manifest_count++
        }
        print > (staging "/Cargo.toml")
        next
    }
    FILENAME == ARGV[2] {
        if ($0 == "[[package]]") package = 0
        if ($0 == "name = \"telete\"") package = 1
        if (package && $0 ~ /^version = "/) {
            if ($0 != "version = \"" old "\"") fail("Cargo.lock version disagrees with Cargo.toml")
            $0 = "version = \"" version "\""
            lock_count++
        }
        print > (staging "/Cargo.lock")
        next
    }
    FILENAME == ARGV[3] {
        if ($0 ~ /^\.TH TELETE /) {
            if (!index($0, "\"Telete " old "\"")) fail("manual version disagrees with Cargo.toml")
            sub(/"Telete [^"]+"/, "\"Telete " version "\"")
            manual_count++
        }
        print > (staging "/telete.1")
    }
    END {
        if (failed) exit 1
        if (manifest_count != 1 || lock_count != 1 || manual_count != 1)
            fail("expected one version record in each of Cargo.toml, Cargo.lock, and docs/telete.1")
        print old " -> " version > (staging "/result")
    }
' "$repo/Cargo.toml" "$repo/Cargo.lock" "$repo/docs/telete.1"

mv -f -- "$staging/Cargo.toml" "$repo/Cargo.toml"
mv -f -- "$staging/Cargo.lock" "$repo/Cargo.lock"
mv -f -- "$staging/telete.1" "$repo/docs/telete.1"
cat "$staging/result"
