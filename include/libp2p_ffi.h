/*
 * libp2p-ffi: rust-libp2p behind a C interface. github.com/gradido/libp2p-ffi
 *
 * The module finds the nodes of a group by the group's key, carries request and response between
 * nodes with failover, and reports what happens as events the caller polls. It holds mechanism and
 * no policy: which operations exist, what a payload means and what a peer class allows belong to
 * the caller. A group is any set of nodes that share a key -- a service running on several servers.
 *
 * What travels on the wire, for a mirror in another language, is in src/wire.rs and
 * src/delegation.rs of that repository.
 *
 * Every function is thread-safe and never blocks on the network, except lp2p_poll with a
 * timeout. Nothing crosses but bytes and lengths: no Rust type, no buffer the caller frees, no
 * pointer into the module after a call returns. A panic is caught at the boundary, becomes
 * LP2P_ERR_PANIC and poisons the handle -- unwinding into a C event loop is undefined behavior.
 *
 * The interface only grows. New fields go at the end of lp2p_options, new event types and
 * status codes get new numbers, and nothing is renamed, renumbered or reused. The option
 * structs nested inside lp2p_options are frozen for the same reason: growing one would move
 * every field after it, so a new knob is a new field at the end of lp2p_options.
 */
#ifndef LIBP2P_FFI_H
#define LIBP2P_FFI_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define LP2P_ABI_VERSION 1

/* Status codes. Negative, so a call that answers a count answers an error the same way. */
#define LP2P_OK 0
#define LP2P_ERR_INVALID_ARGUMENT -1
#define LP2P_ERR_BUFFER_TOO_SMALL -2
#define LP2P_ERR_NO_MEMORY -3
#define LP2P_ERR_NETWORK -4
#define LP2P_ERR_LIMITED -5
/* The call is valid but this build cannot do it: not implemented yet, or switched off. */
#define LP2P_ERR_UNAVAILABLE -6
#define LP2P_ERR_SHUT_DOWN -7
#define LP2P_ERR_PANIC -99

/* An ed25519 public key: a group, or a node -- the node's key is its libp2p peer id. */
typedef uint8_t lp2p_key[32];

/* A delegation: node key (32), group key (32), expiry in unix milliseconds, big endian (8, 0 for
 * none), and the group key's ed25519 signature (64) over
 * "libp2p-ffi delegation v1" || node key || group key || expiry. */
#define LP2P_DELEGATION_BYTES 136

typedef struct lp2p lp2p;

/* A token bucket: `amount` tokens every `interval_ms`, at most `burst` held. */
typedef struct lp2p_rate {
    uint32_t amount;
    uint32_t interval_ms;
    uint32_t burst;
} lp2p_rate;

/* Frozen. libp2p's own relay limits; there is no bytes-per-second cap, the bound is what these
 * multiply to. The server side applies only to a node that is not PRIVATE. A rate with any zero
 * field switches that limiter off. */
typedef struct lp2p_relay_options {
    uint8_t server; /* serve as a relay when reachable */
    uint8_t client; /* reserve a relay when not reachable */
    uint32_t max_reservations;
    uint32_t max_reservations_per_peer;
    uint32_t reservation_duration_s;
    uint32_t max_circuits;
    uint32_t max_circuits_per_peer;
    uint32_t max_circuit_duration_s;
    uint64_t max_circuit_bytes;
    lp2p_rate circuits_per_peer;
    lp2p_rate circuits_per_ip;
} lp2p_relay_options;

/* Frozen. The announcement: a gossipsub message the node signs, published when the caller sets a
 * payload that differs from the last one -- no heartbeat. Receivers check the delegation inside
 * before they report or forward it, drop it for a blocked group, and pass on at most three per
 * node in reserve and one every ten seconds after that. */
typedef struct lp2p_announce_options {
    uint8_t enabled;   /* on by default */
    const char *topic; /* NULL: "<dht_protocol>/announce" */
    uint32_t max_payload_bytes;
} lp2p_announce_options;

typedef struct lp2p_options {
    /* sizeof(lp2p_options) as the caller compiled it. A newer module reads the fields the caller
     * knows and defaults the rest. lp2p_options_default sets it. */
    uint32_t struct_size;
    /* This node's key. Never the group key: the group key signs the delegation and nothing on
     * the node needs it. */
    uint8_t node_seed[32];
    /* "node key X belongs to group Y until T", signed by the group key: LP2P_DELEGATION_BYTES,
     * made with lp2p_delegation_sign or any ed25519 implementation. It travels in every request
     * and response, and the module checks the other side's on each. lp2p_start refuses one that
     * does not name this node, does not match `group` or has expired. */
    const uint8_t *delegation;
    size_t delegation_len;
    lp2p_key group;
    /* Multiaddrs, e.g. /ip4/0.0.0.0/tcp/5000 and /ip4/0.0.0.0/udp/5000/quic-v1. Every string
     * below is copied by lp2p_start and may be freed afterwards. */
    const char *const *listen_addrs;
    size_t listen_addr_count;
    const char *dht_protocol;
    /* The index into this list is the protocol id every RPC call and event carries. */
    const char *const *rpc_protocols;
    size_t rpc_protocol_count;
    uint32_t rpc_max_request_bytes;
    uint32_t rpc_max_response_bytes;
    uint32_t rpc_timeout_ms;
    uint8_t quic;
    uint8_t dcutr;   /* upgrade a relayed connection to a direct one by hole punching */
    uint8_t autonat; /* answer other nodes' dial-back probes, and probe itself when UNKNOWN */
    /* 0 means no limit of the module's own. */
    uint32_t max_connections;
    uint32_t max_connections_per_peer;
    uint32_t max_pending_incoming;
    lp2p_relay_options relay;
    lp2p_announce_options announce;
    /* Bound of the internal event queue. What does not fit is dropped and reported as
     * LP2P_EV_OVERFLOW, never silently. */
    size_t event_queue_bytes;
    /* LP2P_REACH_*: whether other nodes can dial this one directly.
     *   PUBLIC   it listens on addresses others can reach -- a server with a public address.
     *            It announces them, and with relay.server it relays for others.
     *   PRIVATE  it cannot be dialed -- behind NAT, no forwarded port. It reserves a slot on up
     *            to two relays among its peers (relay.client), announces only the relayed
     *            addresses, and never relays for others.
     *   UNKNOWN  the default. With autonat, AutoNAT decides: the node starts as PUBLIC, asks
     *            connected peers to dial it back, and switches to PRIVATE -- or back -- by what
     *            they report, with LP2P_EV_REACHABILITY each time. Without autonat it stays
     *            PUBLIC. A configured PUBLIC or PRIVATE is never overridden. */
    uint8_t reachability;
} lp2p_options;

/* Events. One record per event, whole records only, each followed by its data. */
#define LP2P_EV_LISTENING 1 /* data: the multiaddr, UTF-8 */
/* reason: LP2P_REACH_*. Once at start with the configured value; again whenever AutoNAT moves a
 * node configured UNKNOWN to PUBLIC or PRIVATE. */
#define LP2P_EV_REACHABILITY 2
/* node; data: the address of the first connection, UTF-8 -- it contains /p2p-circuit when that
 * connection is relayed. */
#define LP2P_EV_PEER_CONNECTED 3
#define LP2P_EV_PEER_DISCONNECTED 4 /* node */
/* id, group and node (delegation checked), protocol; data: payload. Answer with respond or
 * reject. */
#define LP2P_EV_RPC_REQUEST 5
#define LP2P_EV_RPC_RESPONSE 6 /* id, group, node that answered, protocol; data: payload */
#define LP2P_EV_RPC_FAILED 7   /* id, group, protocol, reason: LP2P_FAIL_* */
/* group, node (delegation checked); data: payload. Whether the group is new is the caller's call. */
#define LP2P_EV_ANNOUNCEMENT 8
#define LP2P_EV_PEER_DISCOVERED 9 /* id of the walk, node; data: multiaddrs, NUL-separated */
#define LP2P_EV_DHT_RESULT 10     /* id; flags: LP2P_EVF_LAST ends the query */
/* group, node, protocol, reason: the LP2P_SCOPE_* that refused a request, or
 * LP2P_LIMITED_BLOCKED. At most ten per second; lp2p_stats.rpc_limited counts all of them. */
#define LP2P_EV_LIMITED 11
#define LP2P_EV_OVERFLOW 12 /* id: how many events were dropped */

#define LP2P_EVF_LAST 1u

#define LP2P_REACH_UNKNOWN 0
#define LP2P_REACH_PUBLIC 1
#define LP2P_REACH_PRIVATE 2

#define LP2P_FAIL_TIMEOUT 1
#define LP2P_FAIL_UNREACHABLE 2 /* no node of the group answered */
#define LP2P_FAIL_REFUSED 3
#define LP2P_FAIL_LIMITED 4

typedef struct lp2p_event {
    uint16_t type;
    uint16_t flags;
    /* The whole record, this header and its data, padded to a multiple of 8. The next record
     * starts `size` bytes after this one. */
    uint32_t size;
    uint64_t id;
    lp2p_key group; /* zero when the event has none */
    lp2p_key node;  /* zero when the event has none */
    uint16_t protocol;
    uint16_t reason;
    uint32_t data_len; /* data follows this header */
} lp2p_event;

/* Peer classes. A class belongs to a group -- every node of a group is in the group's class --
 * and the numbers between these two mean whatever the caller decides. */
#define LP2P_CLASS_UNKNOWN 0 /* every group nobody classified */
#define LP2P_CLASS_BLOCKED 255
#define LP2P_LIMITED_BLOCKED 255 /* the reason of an LP2P_EV_LIMITED for a blocked group */

#define LP2P_SCOPE_PEER 0
#define LP2P_SCOPE_IP_PREFIX 1 /* IPv4 /24, IPv6 /56; not applied to relayed connections */
#define LP2P_SCOPE_GLOBAL 2

#define LP2P_PROTOCOL_ANY 0xffff

typedef struct lp2p_stats {
    uint32_t size; /* set by the caller; the module fills what fits */
    uint32_t connections;
    uint32_t routing_table_peers;
    uint32_t reserved;
    uint64_t rpc_in;
    uint64_t rpc_out;
    uint64_t rpc_limited;
    uint64_t events_dropped;
} lp2p_stats;

uint32_t lp2p_abi_version(void);

/** Fills @p opt with the module's defaults and sets struct_size. */
void lp2p_options_default(lp2p_options *opt);

/**
 * Starts the node on the module's own threads. Listening has begun when this returns; the
 * addresses arrive as LP2P_EV_LISTENING.
 */
int32_t lp2p_start(const lp2p_options *opt, lp2p **out);

/** Stops the node and frees everything the handle owns. @p node is invalid afterwards. */
int32_t lp2p_shutdown(lp2p *node);

/**
 * Copies whole event records into @p buf and answers the bytes written, 0 if nothing arrived
 * within @p timeout_ms (0: do not wait), or a negative status. A thread that has nothing else
 * to do waits here.
 *
 * A record larger than @p cap stays queued and the call answers LP2P_ERR_BUFFER_TOO_SMALL: size
 * the buffer for the largest payload, rpc_max_request_bytes or rpc_max_response_bytes, plus
 * sizeof(lp2p_event) and 8. After lp2p_shutdown has begun, a drained queue answers
 * LP2P_ERR_SHUT_DOWN; after a panic inside the module, LP2P_ERR_PANIC.
 */
int32_t lp2p_poll(lp2p *node, uint8_t *buf, size_t cap, int32_t timeout_ms);

/**
 * Calls @p group. The module looks up the group's nodes, checks their delegations and fails over
 * from one to the next; @p node_key pins one, NULL means any. The answer arrives as
 * LP2P_EV_RPC_RESPONSE or LP2P_EV_RPC_FAILED with the id written to @p request_id. Every
 * operation carried this way must be idempotent: failover repeats calls.
 */
int32_t lp2p_rpc_request(lp2p *node, const lp2p_key group, const lp2p_key node_key,
                         uint16_t protocol, const uint8_t *data, size_t len, uint32_t timeout_ms,
                         uint64_t *request_id);
/** Answers an LP2P_EV_RPC_REQUEST. Asynchronous: an id that is unknown or already answered is
 * dropped, not reported. */
int32_t lp2p_rpc_respond(lp2p *node, uint64_t request_id, const uint8_t *data, size_t len);
/** Closes the request's stream without an answer; the caller's failover moves on at once. */
int32_t lp2p_rpc_reject(lp2p *node, uint64_t request_id);

/** Policy from outside. */

/** Puts @p group in @p peer_class; LP2P_CLASS_UNKNOWN takes it out again. Applies to requests
 * that arrive from then on. */
int32_t lp2p_peer_set_class(lp2p *node, const lp2p_key group, uint8_t peer_class);

/**
 * Sets the limit for one class, one scope and one protocol index or LP2P_PROTOCOL_ANY, replacing
 * the one that was there; a rate with any zero field removes it. Without a limit a class is not
 * limited. A request is handed to the caller only if every limit that matches it has a token
 * left; otherwise it is refused -- the requester sees LP2P_FAIL_REFUSED -- and reported as
 * LP2P_EV_LIMITED.
 *
 * The check happens when a request has arrived and its delegation is verified, which is when its
 * group, and so its class, is known: the request has been read by then, bounded by
 * rpc_max_request_bytes.
 */
int32_t lp2p_limit_set(lp2p *node, uint8_t peer_class, uint8_t scope, uint16_t protocol,
                       lp2p_rate rate);

/**
 * Sets what this node announces, and announces it when it differs from what was announced last.
 * Without a subscribed peer the announcement waits and goes out as soon as one appears. A node
 * announces nothing until the caller sets a payload, typically right after lp2p_start.
 * LP2P_ERR_UNAVAILABLE when announcements are off; LP2P_ERR_INVALID_ARGUMENT above
 * announce.max_payload_bytes.
 */
int32_t lp2p_announce_set_payload(lp2p *node, const uint8_t *data, size_t len);

/** The network. */
int32_t lp2p_add_address(lp2p *node, const lp2p_key node_key, const char *multiaddr);
int32_t lp2p_dht_bootstrap(lp2p *node, uint64_t *query_id);
/** Optional, and only when the caller asks: every peer the walk meets is reported. */
int32_t lp2p_dht_random_walk(lp2p *node, uint64_t *query_id);
/** A sample of the routing table for a bootstrap answer: event records, as lp2p_poll writes. */
int32_t lp2p_routing_sample(lp2p *node, uint8_t *buf, size_t cap, uint32_t max_peers);
/** Fills as much of @p out as out->size says the caller knows. */
int32_t lp2p_stats_get(const lp2p *node, lp2p_stats *out);

/** Keys and delegations, for whoever holds a group key. None of these touches the network. */
int32_t lp2p_key_from_seed(const uint8_t seed[32], lp2p_key out);
int32_t lp2p_delegation_sign(const uint8_t group_seed[32], const lp2p_key node_key,
                             uint64_t expires_ms, uint8_t out[LP2P_DELEGATION_BYTES]);
/** Checks signature and expiry; @p now_ms 0 means the system clock. Writes the delegation's keys
 * into the outputs that are not NULL. */
int32_t lp2p_delegation_verify(const uint8_t *delegation, size_t len, uint64_t now_ms,
                               lp2p_key group_out, lp2p_key node_out);

#ifdef __cplusplus
}
#endif

#endif /* LIBP2P_FFI_H */
