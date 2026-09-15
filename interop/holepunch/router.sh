#!/bin/sh
# NAT=cone       MASQUERADE: the router keeps a LAN port's mapping for every destination, so the
#                address the relay observed is the one a peer can punch through
# NAT=symmetric  MASQUERADE --random-fully: a new mapping per destination, so the observed address
#                is useless to a peer and hole punching has to fail
set -eu
interface_of() {
    ip -o -4 addr show | awk -v prefix="$1" 'index($4, prefix) == 1 { print $2 }'
}
public=$(interface_of "$PUBLIC_PREFIX")
lan=$(interface_of "$LAN_PREFIX")

# Unsolicited packets to the router itself are dropped, not answered: a home router does not send
# a reset for a port it never opened. A reset would kill a hole punch whose first SYN simply
# arrived before the other side's.
iptables -A INPUT -i "$public" -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT
iptables -A INPUT -i "$public" -j DROP

iptables -P FORWARD DROP
iptables -A FORWARD -i "$lan" -o "$public" -j ACCEPT
iptables -A FORWARD -i "$public" -o "$lan" -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT
case "${NAT:-cone}" in
    symmetric) iptables -t nat -A POSTROUTING -o "$public" -j MASQUERADE --random-fully ;;
    *) iptables -t nat -A POSTROUTING -o "$public" -j MASQUERADE ;;
esac
echo "router: ${NAT:-cone} NAT from $lan to $public"
exec sleep infinity
