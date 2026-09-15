//! The `extern "C"` functions. This is the only module allowed `unsafe`: it turns the caller's
//! pointers and lengths into Rust values, and nothing else in the crate ever sees a raw pointer.
//!
//! Every function catches panics and answers `LP2P_ERR_PANIC` instead of unwinding into C.

use std::ffi::CStr;
use std::os::raw::c_char;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::Ordering;
use std::time::Duration;

use libp2p::{Multiaddr, StreamProtocol};

use crate::abi::*;
use crate::delegation::{Delegation, now_ms};
use crate::keys;
use crate::node::{AnnounceConfig, Command, Config, Node, Reachability, RelayConfig, TokenBucket};

/// The handle C holds.
#[allow(non_camel_case_types)]
pub struct lp2p {
    node: Node,
}

fn guard(f: impl FnOnce() -> i32) -> i32 {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(LP2P_ERR_PANIC)
}

unsafe fn string(p: *const c_char) -> Result<String, i32> {
    if p.is_null() {
        return Err(LP2P_ERR_INVALID_ARGUMENT);
    }
    // SAFETY: the caller passes a NUL-terminated string that lives for the call.
    unsafe { CStr::from_ptr(p) }
        .to_str()
        .map(str::to_owned)
        .map_err(|_| LP2P_ERR_INVALID_ARGUMENT)
}

unsafe fn strings(p: *const *const c_char, count: usize) -> Result<Vec<String>, i32> {
    if count == 0 {
        return Ok(Vec::new());
    }
    if p.is_null() {
        return Err(LP2P_ERR_INVALID_ARGUMENT);
    }
    // SAFETY: the caller passes `count` string pointers.
    let list = unsafe { std::slice::from_raw_parts(p, count) };
    list.iter().map(|&s| unsafe { string(s) }).collect()
}

unsafe fn bytes<'a>(p: *const u8, len: usize) -> Result<&'a [u8], i32> {
    if len == 0 {
        return Ok(&[]);
    }
    if p.is_null() {
        return Err(LP2P_ERR_INVALID_ARGUMENT);
    }
    // SAFETY: the caller passes `len` readable bytes that live for the call.
    Ok(unsafe { std::slice::from_raw_parts(p, len) })
}

unsafe fn key(p: *const u8) -> Result<lp2p_key, i32> {
    if p.is_null() {
        return Err(LP2P_ERR_INVALID_ARGUMENT);
    }
    // SAFETY: an lp2p_key parameter is 32 readable bytes.
    Ok(unsafe { *p.cast::<lp2p_key>() })
}

unsafe fn handle<'a>(node: *const lp2p) -> Result<&'a Node, i32> {
    if node.is_null() {
        return Err(LP2P_ERR_INVALID_ARGUMENT);
    }
    // SAFETY: a non-null handle came from lp2p_start and has not been shut down.
    Ok(unsafe { &(*node).node })
}

fn status(result: Result<i32, i32>) -> i32 {
    result.unwrap_or_else(|e| e)
}

#[unsafe(no_mangle)]
pub extern "C" fn lp2p_abi_version() -> u32 {
    LP2P_ABI_VERSION
}

/// # Safety
/// `opt` is null or points to a writable `lp2p_options`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lp2p_options_default(opt: *mut lp2p_options) {
    if !opt.is_null() {
        // SAFETY: checked for null; the caller owns the struct.
        unsafe { opt.write(default_options()) };
    }
}

/// 0 in a limit field means no limit of the module's own.
fn limit(value: u32) -> Option<u32> {
    (value != 0).then_some(value)
}

/// libp2p's relay limiter holds `limit` tokens and adds one every `interval`; `amount` per
/// `interval_ms` becomes one per `interval_ms / amount`. Any zero switches the limiter off.
fn token_bucket(rate: lp2p_rate) -> Option<TokenBucket> {
    let burst = std::num::NonZeroU32::new(rate.burst)?;
    if rate.amount == 0 || rate.interval_ms == 0 {
        return None;
    }
    let interval = Duration::from_millis((rate.interval_ms / rate.amount).max(1) as u64);
    Some(TokenBucket { burst, interval })
}

unsafe fn config_from(o: &lp2p_options) -> Result<Config, i32> {
    let delegation = Delegation::parse(unsafe { bytes(o.delegation, o.delegation_len)? })
        .map_err(|_| LP2P_ERR_INVALID_ARGUMENT)?;
    if delegation.group != o.group {
        return Err(LP2P_ERR_INVALID_ARGUMENT);
    }
    let listen = unsafe { strings(o.listen_addrs, o.listen_addr_count)? }
        .iter()
        .map(|s| s.parse::<Multiaddr>().map_err(|_| LP2P_ERR_INVALID_ARGUMENT))
        .collect::<Result<Vec<_>, _>>()?;
    let dht_protocol = StreamProtocol::try_from_owned(unsafe { string(o.dht_protocol)? })
        .map_err(|_| LP2P_ERR_INVALID_ARGUMENT)?;
    let rpc_protocols = unsafe { strings(o.rpc_protocols, o.rpc_protocol_count)? };
    let announce = if o.announce.enabled != 0 {
        let topic = if o.announce.topic.is_null() {
            // Derived from the DHT protocol, so two networks never share an announcement topic.
            format!("{}/announce", dht_protocol.as_ref())
        } else {
            unsafe { string(o.announce.topic)? }
        };
        if topic.is_empty() {
            return Err(LP2P_ERR_INVALID_ARGUMENT);
        }
        Some(AnnounceConfig {
            topic,
            max_payload_bytes: o.announce.max_payload_bytes as usize,
        })
    } else {
        None
    };
    if rpc_protocols.len() > u16::MAX as usize || rpc_protocols.iter().any(|p| p.is_empty() || p.len() > 255)
    {
        return Err(LP2P_ERR_INVALID_ARGUMENT);
    }
    if o.rpc_max_request_bytes == 0 || o.rpc_max_response_bytes == 0 || o.rpc_timeout_ms == 0 {
        return Err(LP2P_ERR_INVALID_ARGUMENT);
    }
    Ok(Config {
        node_seed: o.node_seed,
        delegation,
        listen,
        dht_protocol,
        rpc_protocols,
        rpc_max_request_bytes: o.rpc_max_request_bytes,
        rpc_max_response_bytes: o.rpc_max_response_bytes,
        rpc_timeout: Duration::from_millis(o.rpc_timeout_ms as u64),
        quic: o.quic != 0,
        dcutr: o.dcutr != 0,
        reachability: match o.reachability {
            LP2P_REACH_PRIVATE => Reachability::Private,
            LP2P_REACH_PUBLIC => Reachability::Public,
            LP2P_REACH_UNKNOWN => Reachability::Unknown,
            _ => return Err(LP2P_ERR_INVALID_ARGUMENT),
        },
        relay: RelayConfig {
            server: o.relay.server != 0,
            client: o.relay.client != 0,
            max_reservations: o.relay.max_reservations as usize,
            max_reservations_per_peer: o.relay.max_reservations_per_peer as usize,
            reservation_duration: Duration::from_secs(o.relay.reservation_duration_s as u64),
            max_circuits: o.relay.max_circuits as usize,
            max_circuits_per_peer: o.relay.max_circuits_per_peer as usize,
            max_circuit_duration: Duration::from_secs(o.relay.max_circuit_duration_s as u64),
            max_circuit_bytes: o.relay.max_circuit_bytes,
            circuits_per_peer: token_bucket(o.relay.circuits_per_peer),
            circuits_per_ip: token_bucket(o.relay.circuits_per_ip),
        },
        max_connections: limit(o.max_connections),
        max_connections_per_peer: limit(o.max_connections_per_peer),
        max_pending_incoming: limit(o.max_pending_incoming),
        announce,
        // Below one megabyte-sized response a queue would drop the first large event it meets.
        event_queue_bytes: o.event_queue_bytes.max(o.rpc_max_response_bytes as usize + 4096),
    })
}

/// # Safety
/// `opt` points to an `lp2p_options` whose `struct_size` says how much of it is readable, and
/// `out` to a writable handle pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lp2p_start(opt: *const lp2p_options, out: *mut *mut lp2p) -> i32 {
    guard(|| {
        if opt.is_null() || out.is_null() {
            return LP2P_ERR_INVALID_ARGUMENT;
        }
        // SAFETY: every options struct starts with its size.
        let size = unsafe { opt.cast::<u32>().read_unaligned() } as usize;
        if size < std::mem::size_of::<u32>() {
            return LP2P_ERR_INVALID_ARGUMENT;
        }
        // Read what the caller's header knew; everything after it keeps this module's default.
        let mut options = default_options();
        let known = size.min(std::mem::size_of::<lp2p_options>());
        // SAFETY: `known` bytes are readable per struct_size and fit into `options`.
        unsafe {
            std::ptr::copy_nonoverlapping(
                opt.cast::<u8>(),
                (&mut options as *mut lp2p_options).cast::<u8>(),
                known,
            )
        };
        let config = match unsafe { config_from(&options) } {
            Ok(config) => config,
            Err(e) => return e,
        };
        match Node::start(config) {
            Ok(node) => {
                // SAFETY: checked for null.
                unsafe { out.write(Box::into_raw(Box::new(lp2p { node }))) };
                LP2P_OK
            }
            Err(e) => e,
        }
    })
}

/// # Safety
/// `node` came from `lp2p_start` and is not used again.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lp2p_shutdown(node: *mut lp2p) -> i32 {
    guard(|| {
        if node.is_null() {
            return LP2P_ERR_INVALID_ARGUMENT;
        }
        // SAFETY: the handle came from Box::into_raw in lp2p_start; dropping it joins the thread.
        drop(unsafe { Box::from_raw(node) });
        LP2P_OK
    })
}

/// # Safety
/// `node` is a live handle and `buf` holds `cap` writable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lp2p_poll(node: *mut lp2p, buf: *mut u8, cap: usize, timeout_ms: i32) -> i32 {
    guard(|| {
        status((|| {
            let node = unsafe { handle(node)? };
            if (buf.is_null() && cap > 0) || timeout_ms < 0 {
                return Err(LP2P_ERR_INVALID_ARGUMENT);
            }
            let buffer: &mut [u8] = if cap == 0 {
                &mut []
            } else {
                // SAFETY: the caller passes `cap` writable bytes.
                unsafe { std::slice::from_raw_parts_mut(buf, cap) }
            };
            Ok(node
                .shared
                .events
                .poll(buffer, Duration::from_millis(timeout_ms as u64)))
        })())
    })
}

/// # Safety
/// `node` is a live handle, `group` 32 readable bytes, `node_key` null or 32 readable bytes,
/// `data` `len` readable bytes and `request_id` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lp2p_rpc_request(
    node: *mut lp2p,
    group: *const u8,
    node_key: *const u8,
    protocol: u16,
    data: *const u8,
    len: usize,
    timeout_ms: u32,
    request_id: *mut u64,
) -> i32 {
    guard(|| {
        status((|| {
            let node = unsafe { handle(node)? };
            let group = unsafe { key(group)? };
            let target = if node_key.is_null() {
                None
            } else {
                Some(unsafe { key(node_key)? })
            };
            let payload = unsafe { bytes(data, len)? }.to_vec();
            if request_id.is_null()
                || protocol as usize >= node.rpc_protocol_count
                || len > node.rpc_max_request_bytes as usize
            {
                return Err(LP2P_ERR_INVALID_ARGUMENT);
            }
            if let Some(target) = &target {
                keys::peer_id(target).ok_or(LP2P_ERR_INVALID_ARGUMENT)?;
            }
            let id = node.next_id();
            let timeout = if timeout_ms == 0 {
                node.rpc_timeout
            } else {
                Duration::from_millis(timeout_ms as u64)
            };
            let sent = node.send(Command::Rpc {
                id,
                group,
                node: target,
                protocol,
                payload,
                timeout,
            });
            if sent == LP2P_OK {
                // SAFETY: checked for null.
                unsafe { request_id.write(id) };
            }
            Ok(sent)
        })())
    })
}

/// # Safety
/// `node` is a live handle and `data` holds `len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lp2p_rpc_respond(
    node: *mut lp2p,
    request_id: u64,
    data: *const u8,
    len: usize,
) -> i32 {
    guard(|| {
        status((|| {
            let node = unsafe { handle(node)? };
            let payload = unsafe { bytes(data, len)? }.to_vec();
            Ok(node.send(Command::Respond {
                id: request_id,
                payload,
            }))
        })())
    })
}

/// # Safety
/// `node` is a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lp2p_rpc_reject(node: *mut lp2p, request_id: u64) -> i32 {
    guard(|| {
        status((|| {
            Ok(unsafe { handle(node)? }.send(Command::Reject { id: request_id }))
        })())
    })
}

/// # Safety
/// `node` is a live handle and `group` 32 readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lp2p_peer_set_class(node: *mut lp2p, group: *const u8, peer_class: u8) -> i32 {
    guard(|| {
        status((|| {
            let node = unsafe { handle(node)? };
            let group = unsafe { key(group)? };
            Ok(node.send(Command::SetClass {
                group,
                class: peer_class,
            }))
        })())
    })
}

/// # Safety
/// `node` is a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lp2p_limit_set(
    node: *mut lp2p,
    peer_class: u8,
    scope: u8,
    protocol: u16,
    rate: lp2p_rate,
) -> i32 {
    guard(|| {
        status((|| {
            let node = unsafe { handle(node)? };
            if scope > LP2P_SCOPE_GLOBAL
                || peer_class == LP2P_CLASS_BLOCKED
                || (protocol != LP2P_PROTOCOL_ANY && protocol as usize >= node.rpc_protocol_count)
            {
                return Err(LP2P_ERR_INVALID_ARGUMENT);
            }
            Ok(node.send(Command::SetLimit {
                class: peer_class,
                scope,
                protocol,
                rate,
            }))
        })())
    })
}

/// # Safety
/// `node` is a live handle and `data` holds `len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lp2p_announce_set_payload(node: *mut lp2p, data: *const u8, len: usize) -> i32 {
    guard(|| {
        status((|| {
            let node = unsafe { handle(node)? };
            let payload = unsafe { bytes(data, len)? }.to_vec();
            let Some(max) = node.announce_max_payload_bytes else {
                return Ok(LP2P_ERR_UNAVAILABLE);
            };
            if payload.len() > max {
                return Err(LP2P_ERR_INVALID_ARGUMENT);
            }
            Ok(node.send(Command::AnnounceSetPayload { payload }))
        })())
    })
}

/// # Safety
/// `node` is a live handle, `node_key` 32 readable bytes, `multiaddr` a NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lp2p_add_address(
    node: *mut lp2p,
    node_key: *const u8,
    multiaddr: *const c_char,
) -> i32 {
    guard(|| {
        status((|| {
            let node = unsafe { handle(node)? };
            let target = unsafe { key(node_key)? };
            keys::peer_id(&target).ok_or(LP2P_ERR_INVALID_ARGUMENT)?;
            let address = unsafe { string(multiaddr)? }
                .parse::<Multiaddr>()
                .map_err(|_| LP2P_ERR_INVALID_ARGUMENT)?;
            Ok(node.send(Command::AddAddress {
                node: target,
                address,
            }))
        })())
    })
}

unsafe fn query(node: *mut lp2p, query_id: *mut u64, make: impl FnOnce(u64) -> Command) -> i32 {
    status((|| {
        let node = unsafe { handle(node)? };
        if query_id.is_null() {
            return Err(LP2P_ERR_INVALID_ARGUMENT);
        }
        let id = node.next_id();
        let sent = node.send(make(id));
        if sent == LP2P_OK {
            // SAFETY: checked for null.
            unsafe { query_id.write(id) };
        }
        Ok(sent)
    })())
}

/// # Safety
/// `node` is a live handle and `query_id` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lp2p_dht_bootstrap(node: *mut lp2p, query_id: *mut u64) -> i32 {
    guard(|| unsafe { query(node, query_id, |id| Command::Bootstrap { id }) })
}

/// # Safety
/// `node` is a live handle and `query_id` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lp2p_dht_random_walk(node: *mut lp2p, query_id: *mut u64) -> i32 {
    guard(|| unsafe { query(node, query_id, |id| Command::RandomWalk { id }) })
}

/// # Safety
/// `node` is a live handle and `buf` holds `cap` writable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lp2p_routing_sample(
    node: *mut lp2p,
    buf: *mut u8,
    cap: usize,
    max_peers: u32,
) -> i32 {
    guard(|| {
        status((|| {
            let node = unsafe { handle(node)? };
            if buf.is_null() && cap > 0 {
                return Err(LP2P_ERR_INVALID_ARGUMENT);
            }
            let (reply, answer) = std::sync::mpsc::channel();
            let sent = node.send(Command::RoutingSample {
                max: max_peers as usize,
                reply,
            });
            if sent != LP2P_OK {
                return Ok(sent);
            }
            let records = answer
                .recv_timeout(Duration::from_secs(5))
                .map_err(|_| LP2P_ERR_SHUT_DOWN)?;
            let mut written = 0usize;
            for record in records {
                let record = record.bytes();
                if written + record.len() > cap {
                    break;
                }
                // SAFETY: `written + record.len()` is within the caller's `cap` bytes.
                unsafe { std::ptr::copy_nonoverlapping(record.as_ptr(), buf.add(written), record.len()) };
                written += record.len();
            }
            Ok(written as i32)
        })())
    })
}

/// # Safety
/// `node` is a live handle and `out` points to an `lp2p_stats` whose `size` field is set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lp2p_stats_get(node: *const lp2p, out: *mut lp2p_stats) -> i32 {
    guard(|| {
        status((|| {
            let node = unsafe { handle(node)? };
            if out.is_null() {
                return Err(LP2P_ERR_INVALID_ARGUMENT);
            }
            // SAFETY: every stats struct starts with its size.
            let size = unsafe { out.cast::<u32>().read_unaligned() } as usize;
            if size < std::mem::size_of::<u32>() {
                return Err(LP2P_ERR_INVALID_ARGUMENT);
            }
            let shared = &node.shared;
            let stats = lp2p_stats {
                size: size as u32,
                connections: shared.connections.load(Ordering::Relaxed),
                routing_table_peers: shared.routing_table_peers.load(Ordering::Relaxed),
                reserved: 0,
                rpc_in: shared.rpc_in.load(Ordering::Relaxed),
                rpc_out: shared.rpc_out.load(Ordering::Relaxed),
                rpc_limited: shared.rpc_limited.load(Ordering::Relaxed),
                events_dropped: shared.events.dropped(),
            };
            let known = size.min(std::mem::size_of::<lp2p_stats>());
            // SAFETY: the caller's struct holds `size` bytes, `known` of which this module fills.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    (&stats as *const lp2p_stats).cast::<u8>(),
                    out.cast::<u8>(),
                    known,
                )
            };
            Ok(LP2P_OK)
        })())
    })
}

/// # Safety
/// `seed` is 32 readable bytes and `out` 32 writable ones.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lp2p_key_from_seed(seed: *const u8, out: *mut u8) -> i32 {
    guard(|| {
        status((|| {
            let seed = unsafe { key(seed)? };
            if out.is_null() {
                return Err(LP2P_ERR_INVALID_ARGUMENT);
            }
            let keypair = keys::keypair_from_seed(&seed).ok_or(LP2P_ERR_INVALID_ARGUMENT)?;
            let public = keys::public_key(&keypair).ok_or(LP2P_ERR_INVALID_ARGUMENT)?;
            // SAFETY: the caller passes 32 writable bytes.
            unsafe { out.cast::<lp2p_key>().write_unaligned(public) };
            Ok(LP2P_OK)
        })())
    })
}

/// # Safety
/// `group_seed` and `node_key` are 32 readable bytes each, `out` LP2P_DELEGATION_BYTES writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lp2p_delegation_sign(
    group_seed: *const u8,
    node_key: *const u8,
    expires_ms: u64,
    out: *mut u8,
) -> i32 {
    guard(|| {
        status((|| {
            let seed = unsafe { key(group_seed)? };
            let node = unsafe { key(node_key)? };
            if out.is_null() {
                return Err(LP2P_ERR_INVALID_ARGUMENT);
            }
            let delegation = Delegation::sign(&seed, &node, expires_ms).ok_or(LP2P_ERR_INVALID_ARGUMENT)?;
            // SAFETY: the caller passes LP2P_DELEGATION_BYTES writable bytes.
            unsafe {
                out.cast::<[u8; LP2P_DELEGATION_BYTES]>()
                    .write_unaligned(delegation.to_bytes())
            };
            Ok(LP2P_OK)
        })())
    })
}

/// # Safety
/// `delegation` holds `len` readable bytes; `group_out` and `node_out` are null or 32 writable
/// bytes each.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lp2p_delegation_verify(
    delegation: *const u8,
    len: usize,
    now_ms_or_zero: u64,
    group_out: *mut u8,
    node_out: *mut u8,
) -> i32 {
    guard(|| {
        status((|| {
            let parsed = Delegation::parse(unsafe { bytes(delegation, len)? })
                .map_err(|_| LP2P_ERR_INVALID_ARGUMENT)?;
            let now = if now_ms_or_zero == 0 {
                now_ms()
            } else {
                now_ms_or_zero
            };
            parsed.verify(now).map_err(|_| LP2P_ERR_INVALID_ARGUMENT)?;
            if !group_out.is_null() {
                // SAFETY: the caller passes 32 writable bytes.
                unsafe { group_out.cast::<lp2p_key>().write_unaligned(parsed.group) };
            }
            if !node_out.is_null() {
                // SAFETY: the caller passes 32 writable bytes.
                unsafe { node_out.cast::<lp2p_key>().write_unaligned(parsed.node) };
            }
            Ok(LP2P_OK)
        })())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rate_becomes_libp2ps_token_bucket() {
        // One circuit per two minutes, thirty in reserve: libp2p's own default.
        let bucket = token_bucket(lp2p_rate {
            amount: 1,
            interval_ms: 120_000,
            burst: 30,
        })
        .unwrap();
        assert_eq!(
            (bucket.burst.get(), bucket.interval),
            (30, Duration::from_secs(120))
        );
        // Ten per second is one every 100 ms.
        let bucket = token_bucket(lp2p_rate {
            amount: 10,
            interval_ms: 1000,
            burst: 5,
        })
        .unwrap();
        assert_eq!(bucket.interval, Duration::from_millis(100));
        // Any zero switches the limiter off rather than blocking everything.
        assert!(
            token_bucket(lp2p_rate {
                amount: 0,
                interval_ms: 1000,
                burst: 5
            })
            .is_none()
        );
        assert!(
            token_bucket(lp2p_rate {
                amount: 1,
                interval_ms: 1000,
                burst: 0
            })
            .is_none()
        );
        assert_eq!(limit(0), None);
        assert_eq!(limit(7), Some(7));
    }
}
