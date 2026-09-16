#!/bin/sh
# Links tests/c/smoke.c against the shipped object with every C compiler it finds -- the system's
# cc and, when present, zig cc -- and runs it.
#
#   scripts/c-smoke.sh            uses dist/host, building it first when it is missing
#
# Unix only. Windows has no object to link this way and no compiler that is on PATH without the
# MSVC environment; .github/workflows/prebuild.yml does the same link there with cl.
set -eu

root=$(cd "$(dirname "$0")" && cd .. && pwd)
dist="$root/dist/host"
[ -f "$dist/libp2p_ffi.o" ] || "$root/scripts/localize.sh"

# What rustc said this object needs, rather than a list that drifts: -lgcc_s -lutil -lrt ... on
# Linux, -lSystem and a framework or two on macOS.
libs=$(cat "$dist/NATIVE_LIBS.txt" 2>/dev/null || echo "-lpthread -ldl -lm")

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

run() {
    name=$1
    shift
    # The libraries come last: a static object's undefined symbols are resolved left to right.
    "$@" -std=c11 -O2 -Wall -Werror -I "$dist" "$root/tests/c/smoke.c" "$dist/libp2p_ffi.o" \
        -o "$work/smoke-$name" $libs
    printf '%s: ' "$name"
    "$work/smoke-$name"
}

run cc "${CC:-cc}"
if command -v zig > /dev/null 2>&1; then
    run zig zig cc -lunwind
elif [ -x "$HOME/.zig-build/zig/0.15.2/zig" ]; then
    run zig "$HOME/.zig-build/zig/0.15.2/zig" cc -lunwind
fi
