#!/bin/sh
# Links tests/c/smoke.c against the shipped object with every C compiler it finds -- the system's
# cc and, when present, zig cc -- and runs it.
#
#   scripts/c-smoke.sh [target-triple]   uses dist/<triple or host>, building it first when it
#                                        is missing
#   SMOKE_LINK_ONLY=1 scripts/c-smoke.sh <triple>
#                                        links but does not run: a cross-built object can be
#                                        linked on this machine and not executed on it
#
# Unix only. Windows has no object to link this way and no compiler that is on PATH without the
# MSVC environment; .github/workflows/prebuild.yml does the same link there with cl.
set -eu

target=${1:-}
root=$(cd "$(dirname "$0")" && cd .. && pwd)
dist="$root/dist/${target:-host}"
[ -f "$dist/libp2p_ffi.o" ] || "$root/scripts/localize.sh" ${target:+"$target"}

# An object built for another architecture still links here; it is the link that is being proven.
arch=""
case "$target" in
    x86_64-apple-darwin) arch="-arch x86_64" ;;
    aarch64-apple-darwin) arch="-arch arm64" ;;
esac

# What rustc said this object needs, rather than a list that drifts: -lgcc_s -lutil -lrt ... on
# Linux, -lSystem and a framework or two on macOS.
libs=$(cat "$dist/NATIVE_LIBS.txt" 2>/dev/null || echo "-lpthread -ldl -lm")

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

run() {
    name=$1
    shift
    # The libraries come last: a static object's undefined symbols are resolved left to right.
    "$@" $arch -std=c11 -O2 -Wall -Werror -I "$dist" "$root/tests/c/smoke.c" "$dist/libp2p_ffi.o" \
        -o "$work/smoke-$name" $libs
    if [ "${SMOKE_LINK_ONLY:-0}" = 1 ]; then
        echo "$name: linked (not run: built for $target)"
        return 0
    fi
    printf '%s: ' "$name"
    "$work/smoke-$name"
}

run cc "${CC:-cc}"
# zig is the second opinion on the same host, not a cross toolchain.
if [ -n "$arch" ]; then
    exit 0
fi
if command -v zig > /dev/null 2>&1; then
    run zig zig cc -lunwind
elif [ -x "$HOME/.zig-build/zig/0.15.2/zig" ]; then
    run zig "$HOME/.zig-build/zig/0.15.2/zig" cc -lunwind
fi
