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
    #
    # -arch is not optional for a partial link, and the way it fails is not a message: without it
    # ld reads the archive as "building for -unknown", ignores it, and then trips over an
    # assertion of its own in the objc pass.
    case "${target:-$(uname -m)}" in
        x86_64*) arch=x86_64 ;;
        aarch64* | arm64*) arch=arm64 ;;
        *) echo "unknown macOS architecture: ${target:-$(uname -m)}" >&2; exit 1 ;;
    esac
    # The LLVM tools of this very rustc: they know Mach-O, and they read its bitcode, which
    # Xcode's own tools may be too old for. Asked from the repository, so rust-toolchain.toml
    # picks the compiler rather than whatever the caller's shell defaults to.
    tools="$(cd "$root" && rustc --print sysroot)/lib/rustlib/$(cd "$root" && rustc -vV | sed -n 's/^host: //p')/bin"
    if [ ! -x "$tools/llvm-objcopy" ]; then
        echo "llvm-objcopy not found in $tools -- rustup component add llvm-tools" >&2
        exit 1
    fi

    # The one symbol that cannot be made local. compiler_builtins is never part of LTO, and its
    # objects refer to _rust_eh_personality as an undefined external. On ELF, objcopy localizes
    # after the partial link, when those references already point at the definition. On Mach-O
    # the export list is applied inside ld -r, the definition becomes private extern, and the
    # references stay behind unbound: "Undefined symbols: _rust_eh_personality" at the caller's
    # link. Renaming it is not possible either -- llvm-objcopy leaves undefined Mach-O symbols
    # alone. So it stays global, but weak: its references resolve as in any partial link, and a
    # second Rust staticlib in the same binary brings its own personality without a duplicate
    # symbol -- the linker keeps one of them.
    "$tools/llvm-objcopy" --weaken-symbol _rust_eh_personality "$lib" "$work/weak.a"
    lib="$work/weak.a"
    printf '%s\n' '_lp2p_*' '_rust_eh_personality' > "$work/api.txt"
    # -platform_version only silences ld's "no platform load command found" in every build that
    # links this object later. Old linkers do not know the flag, so a refusal falls back rather
    # than failing the release over a warning.
    sdk=$(xcrun --show-sdk-version 2> /dev/null || echo 11.0)
    ld -r -arch "$arch" -platform_version macos 11.0 "$sdk" -all_load "$lib" \
        -exported_symbols_list "$work/api.txt" -o "$work/all.o" 2> "$work/ld.err" || {
        cat "$work/ld.err" >&2
        ld -r -arch "$arch" -all_load "$lib" -exported_symbols_list "$work/api.txt" -o "$work/all.o"
    }
    # The __LLVM segment is the bitcode rustc carries for LTO -- what -R .llvmbc removes on ELF.
    # Nothing links against it from here on, it is most of the size, and it is not inert: Xcode's
    # nm reads it and fails on it, because the bitcode is LLVM 20 from rustc and the tool is LLVM
    # 15 ("Unknown attribute kind"). A consumer's toolchain would meet the same thing.
    if xcrun --find bitcode_strip > /dev/null 2>&1 &&
        xcrun bitcode_strip -r "$work/all.o" -o "$work/nobitcode.o" 2> /dev/null; then
        mv "$work/nobitcode.o" "$work/all.o"
    else
        echo "note: bitcode_strip is not available; the object keeps its __LLVM segment" >&2
    fi
    cp "$work/all.o" "$out/libp2p_ffi.o"
    # No `strip -x` here, however tempting the size is. -exported_symbols_list has already made
    # everything but the lp2p_ names private extern, which is what keeps two Rust staticlibs from
    # colliding -- and private extern is *local*, so -x deletes it. Including
    # _rust_eh_personality, which the unwind tables point at: the object then links with
    # "Undefined symbols: _rust_eh_personality". ELF has objcopy --strip-unneeded, which keeps
    # what relocations need; Mach-O's strip has no such promise.
    artifact=libp2p_ffi.o
    expected=$(grep -c '^int32_t lp2p_\|^uint32_t lp2p_\|^void lp2p_' "$root/include/libp2p_ffi.h")
    if "$tools/llvm-nm" "$out/$artifact" > "$work/symbols.txt" 2> "$work/nm.err"; then
        exported=$(grep -c ' T _lp2p_' "$work/symbols.txt" || true)
        # What the smoke link would find out a step later, said here with its reason.
        if grep -q ' U _rust_eh_personality$' "$work/symbols.txt"; then
            echo "the object still refers to _rust_eh_personality without binding it to its own" >&2
            echo "definition; a caller's link would fail with an undefined symbol." >&2
            exit 1
        fi
    else
        # Counting symbols is the quick check, not the proof: what proves this object is the
        # smoke link that follows it, and a symbol that did not stay global fails that link with
        # "undefined symbol". So a tool that cannot read the file says so and steps aside.
        echo "note: nm could not read the object, so the symbol count is skipped." >&2
        echo "      The C link in scripts/c-smoke.sh is what has to pass. nm said:" >&2
        sed 's/^/      /' "$work/nm.err" >&2
        exported=$expected
    fi
    ;;
coff)
    cp "$base/libp2p_ffi.lib" "$out/libp2p_ffi.lib"
    artifact=libp2p_ffi.lib
    # rustc names the libraries the object needs but not where it found them, and not all of them
    # are Windows': the windows-targets crate ships its own import library, windows.0.52.0.lib,
    # inside the registry. A caller with nothing but this archive cannot link without it, so
    # whatever is not on the linker's own search path travels with the artifact.
    for token in $(cat "$out/NATIVE_LIBS.txt"); do
        case "$token" in *.lib) ;; *) continue ;; esac
        [ -f "$out/$token" ] && continue
        found=$(find "${CARGO_HOME:-$HOME/.cargo}/registry/src" -name "$token" 2> /dev/null | head -n 1)
        if [ -n "$found" ]; then
            cp "$found" "$out/$token"
            echo "carried along: $token"
        fi
    done
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
# Everything in the directory, whatever the platform put there.
(cd "$out" && rm -f SHA256SUMS && set -- * && sums "$@" > SHA256SUMS)
echo "$out/$artifact: $exported global symbols, $(du -h "$out/$artifact" | cut -f1)"
echo "link with: $(cat "$out/NATIVE_LIBS.txt")"
