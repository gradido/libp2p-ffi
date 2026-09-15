/* Prints the layout of every struct in libp2p_ffi.h as the C compiler sees it, one
 * "name value" per line. tests/abi_layout.rs compares it with the Rust definitions. */
#include <stddef.h>
#include <stdio.h>

#include "libp2p_ffi.h"

#define SIZE(T) printf("sizeof.%s %zu\n", #T, sizeof(T))
#define OFF(T, f) printf("offsetof.%s.%s %zu\n", #T, #f, offsetof(T, f))

int main(void)
{
    SIZE(lp2p_rate);
    SIZE(lp2p_relay_options);
    OFF(lp2p_relay_options, max_circuit_bytes);
    OFF(lp2p_relay_options, circuits_per_ip);
    SIZE(lp2p_announce_options);
    OFF(lp2p_announce_options, max_payload_bytes);
    SIZE(lp2p_options);
    OFF(lp2p_options, node_seed);
    OFF(lp2p_options, delegation);
    OFF(lp2p_options, group);
    OFF(lp2p_options, listen_addrs);
    OFF(lp2p_options, dht_protocol);
    OFF(lp2p_options, rpc_protocols);
    OFF(lp2p_options, rpc_max_request_bytes);
    OFF(lp2p_options, quic);
    OFF(lp2p_options, autonat);
    OFF(lp2p_options, max_connections);
    OFF(lp2p_options, relay);
    OFF(lp2p_options, announce);
    OFF(lp2p_options, event_queue_bytes);
    OFF(lp2p_options, reachability);
    SIZE(lp2p_event);
    OFF(lp2p_event, id);
    OFF(lp2p_event, group);
    OFF(lp2p_event, node);
    OFF(lp2p_event, protocol);
    OFF(lp2p_event, data_len);
    SIZE(lp2p_stats);
    OFF(lp2p_stats, rpc_in);
    OFF(lp2p_stats, events_dropped);
    printf("value.LP2P_DELEGATION_BYTES %d\n", LP2P_DELEGATION_BYTES);
    printf("value.LP2P_ABI_VERSION %d\n", LP2P_ABI_VERSION);
    return 0;
}
