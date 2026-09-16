#!/bin/sh
# js-libp2p under Bun against libp2p-ffi.
#
#   interop/js/run.sh            the interop tests on loopback, then the compiled-binary check
#   VERBOSE=1 interop/js/run.sh  also every line the Rust peer prints
#
# Builds examples/interop_peer, installs the pinned js-libp2p packages, runs interop.test.ts with
# the Rust peer as counterpart, and builds compile-check.ts into one executable the way gradido2's
# bundle is built, then runs it from outside the package directory.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)

cargo build --example interop_peer --manifest-path "$root/Cargo.toml"
export INTEROP_PEER="${CARGO_TARGET_DIR:-$root/target}/debug/examples/interop_peer"

cd "$here"
bun install --frozen-lockfile
bun test interop.test.ts
bun run build-compile-check.ts
cd /tmp && "$here/dist/compile-check"
