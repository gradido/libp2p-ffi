#!/bin/sh
# NAT tests in Docker, without root: hole punching and AutoNAT, Rust and js-libp2p in any pairing.
#
#   interop/holepunch/run.sh holepunch [pairs] [transports] [nats]
#       pairs       dialer-listener: rust-rust rust-js js-rust js-js      (default: all)
#       transports  tcp quic                                              (default: both)
#       nats        cone symmetric                                        (default: both)
#   interop/holepunch/run.sh autonat [pairs] [transports]
#       pairs       client-server: rust-rust rust-js js-rust js-js        (default: all)
#   interop/holepunch/run.sh                                              both, everything
#
# TRUST_OBSERVED=1 lets the js nodes offer their observed addresses for hole punching.
#
# RELAY=js puts a js relay into the hole-punching runs. VERBOSE=1 prints every container's log,
# LP2P_TRACE="libp2p_dcutr=debug" adds rust-libp2p's tracing, DEBUG="libp2p:dcutr*" js-libp2p's.
#
# Every line ends in `as-expected` or `UNEXPECTED`; the script exits non-zero when any line is
# UNEXPECTED. What is expected is written down in expect_holepunch and expect_autonat below,
# including the combinations that are expected to fail and why.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
all_pairs="rust-rust rust-js js-rust js-js"
mode=${1:-all}

build() {
    cargo build --release --example holepunch --manifest-path "$root/Cargo.toml"
    mkdir -p "$here/bin"
    cp "${CARGO_TARGET_DIR:-$root/target}/release/examples/holepunch" "$here/bin/holepunch"
    (cd "$root/interop/js" && bun install --frozen-lockfile > /dev/null && bun run build-holepunch.ts "$here/bin/holepunch-js")
    docker compose -f "$here/docker-compose.yml" --profile holepunch --profile autonat build --quiet
}

compose() {
    docker compose -f "$here/docker-compose.yml" "$@"
}

# Hole punching: exit 0 direct, 1 relayed. $1 pair (dialer-listener), $2 transport, $3 nat.
expect_holepunch() {
    case "$1:$2:$3:${TRUST_OBSERVED:-}" in
        # A new mapping per destination: nothing can punch through, the call stays relayed.
        *:*:symmetric:*) echo 1 ;;
        rust-rust:*:cone:*) echo 0 ;;
        # js-libp2p's DCUtR offers only addresses AutoNAT verified, and behind NAT it verifies none:
        # a js node has nothing to offer, and a punch involving one never starts.
        *:*:cone:) echo 1 ;;
        # With TRUST_OBSERVED=1 the js nodes offer their observed addresses as rust-libp2p does.
        # Only a js dialer towards a rust listener over QUIC gets through: js-libp2p's QUIC dials
        # from a new UDP socket each time, so its observed port is not the one a peer could punch
        # to, and its TCP cannot reuse the listen port for a dial at all.
        js-rust:quic:cone:1) echo 0 ;;
        *) echo 1 ;;
    esac
}

# AutoNAT: exit 0 public, 1 private, 2 no verdict. $1 client impl, $2 server impl, $3 where.
expect_autonat() {
    case "$1:$2:$3" in
        rust:rust:public) echo 0 ;;
        rust:rust:nat) echo 1 ;;
        js:rust:public) echo 0 ;;
        # js-libp2p has no private verdict: an observed address is dropped only after 8 failed
        # dial-backs from 8 different /8 networks. With one server it stays unverified.
        js:rust:nat) echo 2 ;;
        # js-libp2p's AutoNAT server dials back with openConnection, which hands it the existing
        # connection to the client; the address check then fails and it closes that connection,
        # taking the request stream with it. No client gets an answer from a js server.
        *:js:*) echo 2 ;;
    esac
}

failed=0
report() { # $1 label, $2 expected, $3 code, $4 result, $5 services whose logs matter
    if [ "$2" = "$3" ]; then verdict=as-expected; else verdict=UNEXPECTED; failed=1; fi
    printf '%-34s exit %s  %-11s %s\n' "$1" "$3" "$verdict" "$4"
    if [ "$verdict" = UNEXPECTED ] || [ -n "${VERBOSE:-}" ]; then
        compose --profile holepunch --profile autonat logs --no-color $5 | sed 's/^/    /'
    fi
}

holepunch() {
    for pair in ${1:-$all_pairs}; do
        for transport in ${2:-tcp quic}; do
            for nat in ${3:-cone symmetric}; do
                export DIALER="${pair%-*}" LISTENER="${pair#*-}" TRANSPORT="$transport" NAT="$nat"
                compose --profile holepunch up -d --force-recreate > /dev/null 2>&1
                code=$(docker wait "$(compose --profile holepunch ps -aq dialer)")
                result=$(compose --profile holepunch logs --no-log-prefix dialer | grep '^dialer: RESULT' | tail -n 1 | sed 's/^dialer: RESULT //')
                report "holepunch $pair ${RELAY:-rust}-relay $transport $nat" \
                    "$(expect_holepunch "$pair" "$transport" "$nat")" "$code" "$result" "relay dialer listener"
                compose --profile holepunch --profile autonat down -t 1 > /dev/null 2>&1
            done
        done
    done
}

autonat() {
    for pair in ${1:-$all_pairs}; do
        for transport in ${2:-tcp quic}; do
            export CLIENT="${pair%-*}" RELAY="${pair#*-}" TRANSPORT="$transport"
            compose --profile autonat up -d --force-recreate > /dev/null 2>&1
            for where in public nat; do
                code=$(docker wait "$(compose --profile autonat ps -aq "client-$where")")
                result=$(compose --profile autonat logs --no-log-prefix "client-$where" | grep 'RESULT' | tail -n 1 | sed 's/^.*RESULT //')
                report "autonat $pair $transport $where" "$(expect_autonat "$CLIENT" "$RELAY" "$where")" "$code" "$result" "relay client-$where"
            done
            compose --profile holepunch --profile autonat down -t 1 > /dev/null 2>&1
        done
    done
}

build
case "$mode" in
    holepunch) shift; holepunch "${1:-}" "${2:-}" "${3:-}" ;;
    autonat) shift; autonat "${1:-}" "${2:-}" ;;
    *) holepunch; autonat ;;
esac
exit "$failed"
