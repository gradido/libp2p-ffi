#!/bin/sh
# Hole punching through NAT, in Docker, without root.
#
#   interop/holepunch/run.sh                    every transport against every NAT
#   interop/holepunch/run.sh quic cone          one combination
#
# Builds the holepunch example on the host, runs the topology in docker-compose.yml once per
# combination and prints one line each. Expected: through cone NAT the connection is upgraded to a
# direct one; through symmetric NAT the call still gets its answer over the relay, and the hole
# punch fails. Exits non-zero when any combination does something else.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
transports=${1:-all}
nats=${2:-all}
[ "$transports" = all ] && transports="tcp quic"
[ "$nats" = all ] && nats="cone symmetric"

cargo build --release --example holepunch --manifest-path "$root/Cargo.toml"
mkdir -p "$here/bin"
cp "${CARGO_TARGET_DIR:-$root/target}/release/examples/holepunch" "$here/bin/holepunch"

compose() {
    docker compose -f "$here/docker-compose.yml" "$@"
}
compose build --quiet

failed=0
for transport in $transports; do
    for nat in $nats; do
        export TRANSPORT="$transport" NAT="$nat"
        compose up -d --force-recreate --quiet-pull > /dev/null 2>&1
        code=$(docker wait "$(compose ps -aq dialer)")
        result=$(compose logs --no-log-prefix dialer | grep '^dialer: RESULT' | tail -n 1 | sed 's/^dialer: RESULT //')

        case "$nat:$code" in
            cone:0 | symmetric:1) verdict=as-expected ;;
            *) verdict=UNEXPECTED; failed=1 ;;
        esac
        printf '%-5s %-9s exit %s  %-11s %s\n' "$transport" "$nat" "$code" "$verdict" "$result"
        if [ "$verdict" = UNEXPECTED ] || [ -n "${VERBOSE:-}" ]; then
            compose logs --no-color | sed 's/^/    /'
        fi
        compose down -t 1 > /dev/null 2>&1
    done
done
exit "$failed"
