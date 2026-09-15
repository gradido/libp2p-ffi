#!/bin/sh
# The LAN networks are internal: the only way out is the router, which GATEWAY names.
set -eu
if [ -n "${GATEWAY:-}" ]; then
    ip route replace default via "$GATEWAY"
fi
exec /usr/local/bin/holepunch
