#!/bin/sh
# The rule that decides whether a merge publishes a release, in one place, so that the pull
# request check and the publishing workflow cannot disagree about it.
#
#   scripts/release-version.sh version                 the version in Cargo.toml
#   scripts/release-version.sh released                the highest version already tagged
#   scripts/release-version.sh check <title> <base>    exits 0 when the pull request is
#                                                      consistent, non-zero with a reason when not
#
# A release is a pull request whose title says "release" and whose Cargo.toml version has not been
# released before. What is hard and what is a note:
#
#   hard   the title says release, so nothing publishes by accident -- a version bump that only
#          prepares the next round stays unpublished
#   hard   v<version> is not tagged yet, and comes after every tag: a released version is never
#          rebuilt and the line never goes backwards
#   hard   a version named in the title is the version in Cargo.toml -- "release v0.0.1" with
#          0.1.0 in the file is a mistake, and it is the one that is easiest to make
#   note   the version is the same as on the base branch. Allowed: the first release of a version
#          that was written down long before it went out, and bumping in one pull request and
#          releasing in another, are both ordinary. The tag rules above are what keep it honest.
set -eu

root=$(cd "$(dirname "$0")" && cd .. && pwd)

version() {
    awk '/^\[package\]/ { p = 1; next } /^\[/ { p = 0 }
         p && /^version[[:space:]]*=/ { gsub(/[",]/, "", $3); print $3; exit }' "$root/Cargo.toml"
}

released() {
    git -C "$root" tag -l 'v*' | sed 's/^v//' | sort -V | tail -n 1
}

# higher A B: A comes after B in version order, and they are not the same.
higher() {
    [ "$1" != "$2" ] && [ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | tail -n 1)" = "$1" ]
}

case "${1:-version}" in
version) version ;;
released) released ;;
check)
    title=${2:-}
    base=${3:-}
    new=$(version)
    case "$new" in
        [0-9]*.[0-9]*.[0-9]*) ;;
        *) echo "Cargo.toml has no x.y.z version: '$new'" >&2; exit 1 ;;
    esac

    old=""
    if [ -n "$base" ]; then
        old=$(git -C "$root" show "$base:Cargo.toml" 2> /dev/null |
            awk '/^\[package\]/ { p = 1; next } /^\[/ { p = 0 }
                 p && /^version[[:space:]]*=/ { gsub(/[",]/, "", $3); print $3; exit }')
    fi

    # Case-insensitive, and anywhere in the title: "Release 0.2.0", "prepare release", "RELEASE".
    if ! printf '%s' "$title" | grep -qi 'release'; then
        if [ -n "$old" ] && [ "$old" != "$new" ]; then
            echo "note: the version moves $old -> $new, but the title does not say release," \
                 "so merging this publishes nothing."
        fi
        echo "not a release: '$title'"
        exit 0
    fi

    # A version in the title -- "release v0.0.1", "Release 0.2.0" -- has to be the one that ships.
    named=$(printf '%s' "$title" | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -n 1 || true)
    if [ -n "$named" ] && [ "$named" != "$new" ]; then
        echo "the title says v$named, Cargo.toml says $new." >&2
        echo "Set version in Cargo.toml to $named, or name $new in the title." >&2
        exit 1
    fi
    if git -C "$root" rev-parse -q --verify "refs/tags/v$new" > /dev/null; then
        echo "v$new is already tagged. A released version is never rebuilt." >&2
        exit 1
    fi
    last=$(released)
    if [ -n "$last" ] && ! higher "$new" "$last"; then
        echo "the highest released version is $last, and $new does not come after it." >&2
        exit 1
    fi
    if [ -n "$old" ] && [ "$old" = "$new" ]; then
        echo "note: the version is unchanged on this branch. Nothing has been released under" \
             "v$new, so this is its first release."
    elif [ -n "$old" ] && higher "$old" "$new"; then
        echo "note: the version moves back, $old -> $new. No tag stands in the way, so it goes" \
             "out -- but check that this is what you meant."
    fi
    echo "release: v$new${old:+ (was $old)}"
    ;;
*) echo "usage: $0 version|released|check <title> <base>" >&2; exit 2 ;;
esac
