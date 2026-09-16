#!/bin/sh
# Builds the release staticlib and turns it into what this module ships, per object format:
#
#   ELF, Mach-O   one relocatable object in which only the lp2p_ functions are global
#   COFF          the staticlib itself; see *Windows* below
#
# Why localize: two Rust staticlibs built with different rustc versions collide on
# rust_eh_personality when linked into one binary, and a #[global_allocator] in one of them fails
# the link or silently takes over the other's allocations. A localized object has neither problem.
#
#   scripts/localize.sh [target-triple]
#       -> dist/<triple or host>/{libp2p_ffi.o | libp2p_ffi.lib}, libp2p_ffi.h,
#          NATIVE_LIBS.txt, SHA256SUMS
#
# Windows: MSVC's toolchain has no partial link -- neither link.exe nor lld-link takes -r -- so
# there is nothing to localize into. The .lib ships as it is. The clash localization avoids is
# ELF's: a COMDAT group named DW.ref.rust_eh_personality. COFF has no counterpart of it, and two
# staticlibs that carry std twice are folded by COMDAT selection. Untested with a second Rust
# staticlib in one binary, and recorded as such in README.md.
set -eu

target=${1:-}
root=$(cd "$(dirname "$0")/.." && pwd)

case "$(uname -s)" in
    Linux*) format=elf ;;
    Darwin*) format=macho ;;
    MINGW* | MSYS* | CYGWIN*) format=coff ;;
    *) echo "unsupported host $(uname -s)" >&2; exit 1 ;;
esac

sums() {
    if command -v sha256sum > /dev/null 2>&1; then sha256sum "$@"; else shasum -a 256 "$@"; fi
}

cargo build --release --lib --manifest-path "$root/Cargo.toml" ${target:+--target "$target"}
base="${CARGO_TARGET_DIR:-$root/target}/${target:+$target/}release"
out="$root/dist/${target:-host}"
mkdir -p "$out"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# What the caller has to put on its link line beside this object. Printed by rustc rather than
# written down here, because it differs per platform and moves with the dependencies.
cargo rustc --release --lib --quiet --manifest-path "$root/Cargo.toml" ${target:+--target "$target"} \
    -- --print native-static-libs 2>&1 | sed -n 's/^note: native-static-libs: *//p' | head -1 \
    > "$out/NATIVE_LIBS.txt"

case "$format" in
elf)
    lib="$base/liblibp2p_ffi.a"
    # Members of a Rust staticlib may share a name, and a plain `ar x` would let the last one
    # overwrite the others. Each member is extracted by its occurrence into a directory of its own.
    ar t "$lib" | awk '{ n[$0]++; print n[$0] "\t" $0 }' | while IFS="$(printf '\t')" read -r count name; do
        case "$name" in *.o) ;; *) continue ;; esac
        mkdir -p "$work/m/$count"
        (cd "$work/m/$count" && ar xN "$count" "$lib" "$name")
    done
    find "$work/m" -name '*.o' > "$work/objects"
    xargs ld -r -o "$work/all.o" < "$work/objects"

    # GNU nm crashes on these objects through its LLVM plugin, which is why readelf lists them.
    readelf -Ws "$work/all.o" | awk '$5 == "GLOBAL" && $7 != "UND" { print $8 }' | grep '^lp2p_' | sort -u > "$work/api.txt"
    [ -s "$work/api.txt" ] || { echo "no lp2p_ symbols found" >&2; exit 1; }

    # `-R .group` is not optional: without it lld refuses the link because every Rust object
    # carries a COMDAT group named DW.ref.rust_eh_personality. .llvmbc and .llvmcmd are LLVM
    # bitcode that std's objects carry for LTO; nothing links against it after this point, and it
    # is most of the archive's size.
    objcopy --keep-global-symbols="$work/api.txt" -R .group -R .llvmbc -R .llvmcmd "$work/all.o" "$out/libp2p_ffi.o"
    # Local symbols the relocations do not need go too; the global lp2p_ ones stay.
    strip --strip-unneeded "$out/libp2p_ffi.o"
    artifact=libp2p_ffi.o
    exported=$(readelf -Ws "$out/$artifact" | awk '$5 == "GLOBAL" && $7 != "UND" { print $8 }' | grep -c '^lp2p_')
    expected=$(wc -l < "$work/api.txt")
    ;;
macho)
    lib="$base/liblibp2p_ffi.a"
    # ld64 takes the archive whole with -all_load, so the duplicate-member dance above is not
    # needed. -exported_symbols_list globs, and everything it does not name becomes private extern.
    echo '_lp2p_*' > "$work/api.txt"
    ld -r -all_load "$lib" -exported_symbols_list "$work/api.txt" -o "$out/libp2p_ffi.o"
    # -x drops the local symbols; the exported ones stay.
    strip -x "$out/libp2p_ffi.o"
    artifact=libp2p_ffi.o
    exported=$(nm -g "$out/$artifact" | grep -c ' T _lp2p_' || true)
    expected=$(grep -c '^int32_t lp2p_\|^uint32_t lp2p_\|^void lp2p_' "$root/include/libp2p_ffi.h")
    ;;
coff)
    cp "$base/libp2p_ffi.lib" "$out/libp2p_ffi.lib"
    artifact=libp2p_ffi.lib
    # No symbol table reader that is there on every Windows runner without the MSVC environment;
    # what proves this artifact is the smoke link in scripts/c-smoke.sh, which fails loudly.
    exported=0
    expected=0
    ;;
esac

if [ "$exported" -lt "$expected" ]; then
    echo "only $exported of $expected lp2p_ symbols are exported" >&2
    exit 1
fi

cp "$root/include/libp2p_ffi.h" "$out/"
(cd "$out" && sums "$artifact" libp2p_ffi.h NATIVE_LIBS.txt > SHA256SUMS)
echo "$out/$artifact: $exported global symbols, $(du -h "$out/$artifact" | cut -f1)"
echo "link with: $(cat "$out/NATIVE_LIBS.txt")"
