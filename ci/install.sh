#!/bin/sh
set -eu

usage() {
    printf '%s\n' 'Usage: ci/install.sh [--prefix PATH]' \
        'Build and install Telete and its manual (default prefix: ~/.local).'
}

prefix=${HOME:?Set HOME to your home directory}/.local
while [ "$#" -gt 0 ]; do
    case "$1" in
        --prefix)
            if [ "$#" -lt 2 ] || [ -z "$2" ]; then
                usage >&2
                exit 2
            fi
            prefix=$2
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            usage >&2
            exit 2
            ;;
    esac
done

case "$prefix" in
    /*) ;;
    *) prefix=$PWD/$prefix ;;
esac
repo=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd -P)
cd "$repo"

# Installation needs a native executable at a known path, regardless of Cargo's
# inherited target-directory or cross-compilation defaults.
native_target=$(rustc -vV | sed -n 's/^host: //p')
if [ -z "$native_target" ]; then
    printf '%s\n' 'Could not determine the native Rust target.' >&2
    exit 1
fi
printf 'Building Telete for %s.\n' "$native_target"
cargo build --release --locked --bin telete \
    --target-dir "$repo/target" --target "$native_target"

bin_dir=$prefix/bin
man_dir=$prefix/share/man/man1
mkdir -p "$bin_dir" "$man_dir"
if [ -d "$bin_dir/telete" ] || [ -d "$man_dir/telete.1" ]; then
    printf '%s\n' 'An installation destination is a directory; remove it before installing.' >&2
    exit 1
fi

bin_stage=
man_stage=
cleanup() {
    if [ -n "$bin_stage" ]; then
        rm -rf "$bin_stage"
    fi
    if [ -n "$man_stage" ]; then
        rm -rf "$man_stage"
    fi
}
trap cleanup 0
trap 'exit 1' HUP INT TERM

# Stage both files beside their destinations before replacing either one. The
# final renames are atomic and replace an existing executable symlink itself.
bin_stage=$(mktemp -d "$bin_dir/.telete-install.XXXXXX")
man_stage=$(mktemp -d "$man_dir/.telete-install.XXXXXX")
cp "$repo/target/$native_target/release/telete" "$bin_stage/telete"
chmod 755 "$bin_stage/telete"
cp "$repo/docs/telete.1" "$man_stage/telete.1"
chmod 644 "$man_stage/telete.1"
version=$("$bin_stage/telete" --version)
mv -f "$man_stage/telete.1" "$man_dir/telete.1"
mv -f "$bin_stage/telete" "$bin_dir/telete"

printf 'Installed %s at %s\n' "$version" "$bin_dir/telete"
printf 'Installed manual at %s\n' "$man_dir/telete.1"
quoted_bin_dir=$(printf '%s' "$bin_dir" | sed "s/'/'\\\\''/g")
printf '%s\n' 'To invoke telete from any directory, add this to your shell configuration once:'
printf "  export PATH='%s':\"\$PATH\"\n" "$quoted_bin_dir"
