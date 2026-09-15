#!/bin/sh
# Links tests/c/smoke.c against the localized object with every C compiler it finds -- the
# system's cc and, when present, zig cc -- and runs it.
#
#   scripts/c-smoke.sh            uses dist/host, building it first when it is missing
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
dist="$root/dist/host"
[ -f "$dist/libp2p_ffi.o" ] || "$root/scripts/localize.sh"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

run() {
    name=$1
    shift
    "$@" -std=c11 -O2 -Wall -Werror -I "$dist" "$root/tests/c/smoke.c" "$dist/libp2p_ffi.o" \
        -o "$work/smoke-$name" -lpthread -ldl -lm
    printf '%s: ' "$name"
    "$work/smoke-$name"
}

run cc "${CC:-cc}"
if command -v zig > /dev/null 2>&1; then
    run zig zig cc -lunwind
elif [ -x "$HOME/.zig-build/zig/0.15.2/zig" ]; then
    run zig "$HOME/.zig-build/zig/0.15.2/zig" cc -lunwind
fi
