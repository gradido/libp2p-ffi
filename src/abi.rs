//! The C types of `include/libp2p_ffi.h`, field for field. `tests/abi_layout.rs` compiles the
//! header and holds both layouts to each other.
#![allow(non_camel_case_types)]

use std::os::raw::c_char;

pub const LP2P_ABI_VERSION: u32 = 1;

pub const LP2P_OK: i32 = 0;
pub const LP2P_ERR_INVALID_ARGUMENT: i32 = -1;
pub const LP2P_ERR_BUFFER_TOO_SMALL: i32 = -2;
pub const LP2P_ERR_NO_MEMORY: i32 = -3;
pub const LP2P_ERR_NETWORK: i32 = -4;
pub const LP2P_ERR_LIMITED: i32 = -5;
pub const LP2P_ERR_UNAVAILABLE: i32 = -6;
pub const LP2P_ERR_SHUT_DOWN: i32 = -7;
pub const LP2P_ERR_PANIC: i32 = -99;

pub const LP2P_DELEGATION_BYTES: usize = 136;

pub type lp2p_key = [u8; 32];

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct lp2p_rate {
    pub amount: u32,
    pub interval_ms: u32,
    pub burst: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct lp2p_relay_options {
    pub server: u8,
    pub client: u8,
    pub max_reservations: u32,
    pub max_reservations_per_peer: u32,
    pub reservation_duration_s: u32,
    pub max_circuits: u32,
    pub max_circuits_per_peer: u32,
    pub max_circuit_duration_s: u32,
    pub max_circuit_bytes: u64,
    pub circuits_per_peer: lp2p_rate,
    pub circuits_per_ip: lp2p_rate,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct lp2p_announce_options {
    pub enabled: u8,
    pub topic: *const c_char,
    pub max_payload_bytes: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct lp2p_options {
    pub struct_size: u32,
    pub node_seed: [u8; 32],
    pub delegation: *const u8,
    pub delegation_len: usize,
    pub group: lp2p_key,
    pub listen_addrs: *const *const c_char,
    pub listen_addr_count: usize,
    pub dht_protocol: *const c_char,
    pub rpc_protocols: *const *const c_char,
    pub rpc_protocol_count: usize,
    pub rpc_max_request_bytes: u32,
    pub rpc_max_response_bytes: u32,
    pub rpc_timeout_ms: u32,
    pub quic: u8,
    pub dcutr: u8,
    pub autonat: u8,
    pub max_connections: u32,
    pub max_connections_per_peer: u32,
    pub max_pending_incoming: u32,
    pub relay: lp2p_relay_options,
    pub announce: lp2p_announce_options,
    pub event_queue_bytes: usize,
    pub reachability: u8,
}

pub const LP2P_EV_LISTENING: u16 = 1;
pub const LP2P_EV_REACHABILITY: u16 = 2;
pub const LP2P_EV_PEER_CONNECTED: u16 = 3;
pub const LP2P_EV_PEER_DISCONNECTED: u16 = 4;
pub const LP2P_EV_RPC_REQUEST: u16 = 5;
pub const LP2P_EV_RPC_RESPONSE: u16 = 6;
pub const LP2P_EV_RPC_FAILED: u16 = 7;
pub const LP2P_EV_ANNOUNCEMENT: u16 = 8;
pub const LP2P_EV_PEER_DISCOVERED: u16 = 9;
pub const LP2P_EV_DHT_RESULT: u16 = 10;
pub const LP2P_EV_LIMITED: u16 = 11;
pub const LP2P_EV_OVERFLOW: u16 = 12;

pub const LP2P_EVF_LAST: u16 = 1;

pub const LP2P_REACH_UNKNOWN: u8 = 0;
pub const LP2P_REACH_PUBLIC: u8 = 1;
pub const LP2P_REACH_PRIVATE: u8 = 2;

pub const LP2P_FAIL_TIMEOUT: u16 = 1;
pub const LP2P_FAIL_UNREACHABLE: u16 = 2;
pub const LP2P_FAIL_REFUSED: u16 = 3;
pub const LP2P_FAIL_LIMITED: u16 = 4;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct lp2p_event {
    pub r#type: u16,
    pub flags: u16,
    pub size: u32,
    pub id: u64,
    pub group: lp2p_key,
    pub node: lp2p_key,
    pub protocol: u16,
    pub reason: u16,
    pub data_len: u32,
}

pub const LP2P_EVENT_HEADER_BYTES: usize = std::mem::size_of::<lp2p_event>();
const _: () = assert!(LP2P_EVENT_HEADER_BYTES == 88);

pub const LP2P_CLASS_UNKNOWN: u8 = 0;
pub const LP2P_CLASS_BLOCKED: u8 = 255;
pub const LP2P_PROTOCOL_ANY: u16 = 0xffff;
/// The reason an LP2P_EV_LIMITED carries when the class is LP2P_CLASS_BLOCKED rather than a scope.
pub const LP2P_LIMITED_BLOCKED: u16 = 255;

pub const LP2P_SCOPE_PEER: u8 = 0;
pub const LP2P_SCOPE_IP_PREFIX: u8 = 1;
pub const LP2P_SCOPE_GLOBAL: u8 = 2;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct lp2p_stats {
    pub size: u32,
    pub connections: u32,
    pub routing_table_peers: u32,
    pub reserved: u32,
    pub rpc_in: u64,
    pub rpc_out: u64,
    pub rpc_limited: u64,
    pub events_dropped: u64,
}

/// The defaults `lp2p_options_default` hands out. The relay values are rust-libp2p's own.
pub fn default_options() -> lp2p_options {
    lp2p_options {
        struct_size: std::mem::size_of::<lp2p_options>() as u32,
        node_seed: [0; 32],
        delegation: std::ptr::null(),
        delegation_len: 0,
        group: [0; 32],
        listen_addrs: std::ptr::null(),
        listen_addr_count: 0,
        dht_protocol: std::ptr::null(),
        rpc_protocols: std::ptr::null(),
        rpc_protocol_count: 0,
        rpc_max_request_bytes: 1 << 20,
        rpc_max_response_bytes: 10 << 20,
        rpc_timeout_ms: 10_000,
        quic: 1,
        dcutr: 1,
        autonat: 1,
        max_connections: 0,
        max_connections_per_peer: 0,
        max_pending_incoming: 0,
        relay: lp2p_relay_options {
            server: 1,
            client: 1,
            max_reservations: 128,
            max_reservations_per_peer: 4,
            reservation_duration_s: 60 * 60,
            max_circuits: 16,
            max_circuits_per_peer: 4,
            max_circuit_duration_s: 2 * 60,
            max_circuit_bytes: 1 << 17,
            circuits_per_peer: lp2p_rate {
                amount: 1,
                interval_ms: 2 * 60 * 1000,
                burst: 30,
            },
            circuits_per_ip: lp2p_rate {
                amount: 1,
                interval_ms: 60 * 1000,
                burst: 60,
            },
        },
        announce: lp2p_announce_options {
            enabled: 1,
            topic: std::ptr::null(),
            max_payload_bytes: 1024,
        },
        event_queue_bytes: 1 << 20,
        reachability: LP2P_REACH_UNKNOWN,
    }
}
