#!/bin/sh
# Builds the release staticlib and turns it into the object this module ships: one relocatable
# object in which only the lp2p_ functions are global.
#
# Why: two Rust staticlibs built with different rustc versions collide on rust_eh_personality when
# linked into one binary, and a #[global_allocator] in one of them fails the link or silently
# takes over the other's allocations. A localized object has neither problem.
#
#   scripts/localize.sh [target-triple]      ->  dist/<triple or host>/libp2p_ffi.o, .h, SHA256SUMS
#
# Needs ar, ld, readelf and objcopy for the target. `-R .group` is not optional: without it lld
# refuses the link because every Rust object carries a COMDAT group named
# DW.ref.rust_eh_personality. GNU nm crashes on these objects through its LLVM plugin, which is why
# readelf lists the symbols.
set -eu

target=${1:-}
root=$(cd "$(dirname "$0")/.." && pwd)
cargo build --release --lib --manifest-path "$root/Cargo.toml" ${target:+--target "$target"}
lib="${CARGO_TARGET_DIR:-$root/target}/${target:+$target/}release/liblibp2p_ffi.a"
out="$root/dist/${target:-host}"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# Members of a Rust staticlib may share a name, and a plain `ar x` would let the last one overwrite
# the others. Each member is extracted by its occurrence into a directory of its own.
ar t "$lib" | awk '{ n[$0]++; print n[$0] "\t" $0 }' | while IFS="$(printf '\t')" read -r count name; do
    case "$name" in *.o) ;; *) continue ;; esac
    mkdir -p "$work/m/$count"
    (cd "$work/m/$count" && ar xN "$count" "$lib" "$name")
done
find "$work/m" -name '*.o' > "$work/objects"
xargs ld -r -o "$work/all.o" < "$work/objects"

readelf -Ws "$work/all.o" | awk '$5 == "GLOBAL" && $7 != "UND" { print $8 }' | grep '^lp2p_' | sort -u > "$work/api.txt"
[ -s "$work/api.txt" ] || { echo "no lp2p_ symbols found" >&2; exit 1; }

mkdir -p "$out"
# .llvmbc and .llvmcmd are LLVM bitcode that std's objects carry for LTO. Nothing links against it
# after this point, and it is most of the archive's size.
objcopy --keep-global-symbols="$work/api.txt" -R .group -R .llvmbc -R .llvmcmd "$work/all.o" "$out/libp2p_ffi.o"
# Local symbols the relocations do not need go too; the global lp2p_ ones stay.
strip --strip-unneeded "$out/libp2p_ffi.o"
cp "$root/include/libp2p_ffi.h" "$out/"
(cd "$out" && sha256sum libp2p_ffi.o libp2p_ffi.h > SHA256SUMS)
echo "$out/libp2p_ffi.o: $(wc -l < "$work/api.txt") global symbols, $(du -h "$out/libp2p_ffi.o" | cut -f1)"
