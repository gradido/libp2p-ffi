//! One node of the hole-punching test in `interop/holepunch`, driven through the C interface.
//!
//! ```text
//! ROLE=relay      PUBLIC, relays for the others and answers their AutoNAT probes
//! ROLE=listener   PRIVATE behind NAT b: reserves on the relay, answers every call with "pong"
//! ROLE=dialer     PRIVATE behind NAT a: calls the listener through the relay, waits for DCUtR
//!                 to upgrade the connection, calls again, prints one RESULT line and exits
//!                 0 hole punched, 1 still relayed (the listener logs why), 2 no answer at all
//! ROLE=autonat-client   UNKNOWN: connects to the relay and lets AutoNAT decide, prints one
//!                 RESULT line and exits 0 public, 1 private, 2 no verdict within a minute
//! TRANSPORT=tcp|quic   RELAY_IP=11.99.0.10   SEED=<byte> for an autonat client
//! ```
//!
//! Keys come from fixed seeds, so every role knows every other role's peer id without talking.

use std::ffi::CString;
use std::io::Write;
use std::os::raw::c_char;
use std::time::{Duration, Instant};

use libp2p::PeerId;
use libp2p::identity::{PublicKey, ed25519};
use libp2p_ffi::abi::*;
use libp2p_ffi::ffi::*;

const PORT: u16 = 4001;
const PROTOCOLS: [&str; 1] = ["/holepunch/echo/1"];

struct Node {
    handle: *mut lp2p,
    key: lp2p_key,
}

struct Event {
    header: lp2p_event,
    data: Vec<u8>,
}

fn say(role: &str, message: impl AsRef<str>) {
    println!("{role}: {}", message.as_ref());
    let _ = std::io::stdout().flush();
}

fn key_of(seed: [u8; 32]) -> lp2p_key {
    let mut key = [0u8; 32];
    assert_eq!(
        unsafe { lp2p_key_from_seed(seed.as_ptr(), key.as_mut_ptr()) },
        LP2P_OK
    );
    key
}

fn peer_id(key: &lp2p_key) -> PeerId {
    PublicKey::from(ed25519::PublicKey::try_from_bytes(key).unwrap()).to_peer_id()
}

fn addresses(transport: &str, ip: &str) -> String {
    match transport {
        "quic" => format!("/ip4/{ip}/udp/{PORT}/quic-v1"),
        _ => format!("/ip4/{ip}/tcp/{PORT}"),
    }
}

fn start(seed: [u8; 32], group_seed: [u8; 32], reachability: u8, transport: &str, autonat: bool) -> Node {
    let key = key_of(seed);
    let mut delegation = [0u8; LP2P_DELEGATION_BYTES];
    assert_eq!(
        unsafe { lp2p_delegation_sign(group_seed.as_ptr(), key.as_ptr(), 0, delegation.as_mut_ptr()) },
        LP2P_OK
    );
    let listen = CString::new(addresses(transport, "0.0.0.0")).unwrap();
    let listen_list = [listen.as_ptr()];
    let dht = CString::new("/holepunch/kad/1").unwrap();
    let protocols: Vec<CString> = PROTOCOLS.iter().map(|p| CString::new(*p).unwrap()).collect();
    let protocol_list: Vec<*const c_char> = protocols.iter().map(|p| p.as_ptr()).collect();

    let mut options = std::mem::MaybeUninit::<lp2p_options>::uninit();
    unsafe { lp2p_options_default(options.as_mut_ptr()) };
    let mut options = unsafe { options.assume_init() };
    options.node_seed = seed;
    options.delegation = delegation.as_ptr();
    options.delegation_len = delegation.len();
    options.group = key_of(group_seed);
    options.listen_addrs = listen_list.as_ptr();
    options.listen_addr_count = 1;
    options.dht_protocol = dht.as_ptr();
    options.rpc_protocols = protocol_list.as_ptr();
    options.rpc_protocol_count = 1;
    options.quic = u8::from(transport == "quic");
    options.dcutr = 1;
    options.autonat = u8::from(autonat);
    options.announce.enabled = 0;
    options.reachability = reachability;

    let mut handle = std::ptr::null_mut();
    let status = unsafe { lp2p_start(&options, &mut handle) };
    assert_eq!(status, LP2P_OK, "lp2p_start");
    Node { handle, key }
}

impl Node {
    fn poll(&self, timeout_ms: i32) -> Vec<Event> {
        let mut buf = vec![0u8; 1 << 16];
        let n = unsafe { lp2p_poll(self.handle, buf.as_mut_ptr(), buf.len(), timeout_ms) };
        let mut events = Vec::new();
        let mut offset = 0usize;
        while n > 0 && offset + LP2P_EVENT_HEADER_BYTES <= n as usize {
            let header: lp2p_event = unsafe { std::ptr::read_unaligned(buf[offset..].as_ptr().cast()) };
            let start = offset + LP2P_EVENT_HEADER_BYTES;
            events.push(Event {
                header,
                data: buf[start..start + header.data_len as usize].to_vec(),
            });
            offset += header.size as usize;
        }
        events
    }

    fn add_address(&self, key: &lp2p_key, address: &str) {
        let address = CString::new(address).unwrap();
        assert_eq!(
            unsafe { lp2p_add_address(self.handle, key.as_ptr(), address.as_ptr()) },
            LP2P_OK
        );
    }

    fn call(&self, group: &lp2p_key, node: &lp2p_key) -> u64 {
        let mut id = 0;
        let payload = b"ping";
        let status = unsafe {
            lp2p_rpc_request(
                self.handle,
                group.as_ptr(),
                node.as_ptr(),
                0,
                payload.as_ptr(),
                4,
                10_000,
                &mut id,
            )
        };
        assert_eq!(status, LP2P_OK);
        id
    }
}

fn describe(event: &Event) -> Option<String> {
    let data = String::from_utf8_lossy(&event.data);
    match event.header.r#type {
        LP2P_EV_LISTENING => Some(format!("listening {data}")),
        LP2P_EV_PEER_CONNECTED => Some(format!("connected {} over {data}", peer_id(&event.header.node))),
        LP2P_EV_HOLE_PUNCH if event.header.reason == 0 => Some(format!("hole punched, direct {data}")),
        LP2P_EV_HOLE_PUNCH => Some(format!("hole punch failed: {data}")),
        LP2P_EV_RPC_FAILED => Some(format!("call failed, reason {}", event.header.reason)),
        LP2P_EV_REACHABILITY => Some(format!("reachability {}", event.header.reason)),
        _ => None,
    }
}

fn main() {
    // LP2P_TRACE="libp2p_dcutr=debug,libp2p_tcp=debug" logs what libp2p does underneath.
    if let Ok(filter) = std::env::var("LP2P_TRACE")
        && !filter.is_empty()
    {
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
            .with_writer(std::io::stderr)
            .init();
    }
    let role = std::env::var("ROLE").unwrap_or_else(|_| "dialer".into());
    let transport = std::env::var("TRANSPORT").unwrap_or_else(|_| "tcp".into());
    let relay_ip = std::env::var("RELAY_IP").unwrap_or_else(|_| "11.99.0.10".into());
    let (relay_seed, listener_seed, dialer_seed) = ([1u8; 32], [2u8; 32], [3u8; 32]);
    let (relay_group, listener_group, dialer_group) = ([0x11u8; 32], [0x22u8; 32], [0x33u8; 32]);
    let relay_key = key_of(relay_seed);
    let relay_address = addresses(&transport, &relay_ip);

    match role.as_str() {
        "relay" => {
            let node = start(relay_seed, relay_group, LP2P_REACH_PUBLIC, &transport, true);
            say(&role, format!("peer {}", peer_id(&node.key)));
            loop {
                for event in node.poll(1000) {
                    if let Some(line) = describe(&event) {
                        say(&role, line);
                    }
                }
            }
        }
        "listener" => {
            let node = start(
                listener_seed,
                listener_group,
                LP2P_REACH_PRIVATE,
                &transport,
                false,
            );
            node.add_address(&relay_key, &relay_address);
            let mut id = 0;
            unsafe { lp2p_dht_bootstrap(node.handle, &mut id) };
            loop {
                for event in node.poll(1000) {
                    if event.header.r#type == LP2P_EV_RPC_REQUEST {
                        unsafe { lp2p_rpc_respond(node.handle, event.header.id, b"pong".as_ptr(), 4) };
                    }
                    if let Some(line) = describe(&event) {
                        say(&role, line);
                    }
                }
            }
        }
        "autonat-client" => {
            let seed = std::env::var("SEED")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(4u8);
            let node = start(
                [seed; 32],
                [seed.wrapping_add(0x40); 32],
                LP2P_REACH_UNKNOWN,
                &transport,
                true,
            );
            node.add_address(&relay_key, &relay_address);
            let mut id = 0;
            unsafe { lp2p_dht_bootstrap(node.handle, &mut id) };
            let deadline = Instant::now() + Duration::from_secs(60);
            let mut verdict = None;
            while verdict.is_none() && Instant::now() < deadline {
                for event in node.poll(500) {
                    if let Some(line) = describe(&event) {
                        say(&role, line);
                    }
                    if event.header.r#type == LP2P_EV_REACHABILITY
                        && event.header.reason != u16::from(LP2P_REACH_UNKNOWN)
                    {
                        verdict = Some(event.header.reason);
                    }
                }
            }
            let (text, code) = match verdict {
                Some(r) if r == u16::from(LP2P_REACH_PUBLIC) => ("public", 0),
                Some(_) => ("private", 1),
                None => ("unknown", 2),
            };
            say(
                &role,
                format!("RESULT impl=rust transport={transport} reachability={text}"),
            );
            unsafe { lp2p_shutdown(node.handle) };
            std::process::exit(code);
        }
        _ => {
            let node = start(dialer_seed, dialer_group, LP2P_REACH_PRIVATE, &transport, false);
            node.add_address(&relay_key, &relay_address);
            let listener_key = key_of(listener_seed);
            let circuit = format!("{relay_address}/p2p/{}/p2p-circuit", peer_id(&relay_key));
            node.add_address(&listener_key, &circuit);
            let listener_group_key = key_of(listener_group);

            // The listener needs a moment to hold its reservation; until then a call fails.
            let deadline = Instant::now() + Duration::from_secs(60);
            let mut answered = false;
            let mut first_connection = String::new();
            let mut punched: Option<Result<String, String>> = None;
            while !answered && Instant::now() < deadline {
                let id = node.call(&listener_group_key, &listener_key);
                let call_deadline = Instant::now() + Duration::from_secs(12);
                'call: while Instant::now() < call_deadline {
                    for event in node.poll(200) {
                        if let Some(line) = describe(&event) {
                            say(&role, line);
                        }
                        match event.header.r#type {
                            LP2P_EV_PEER_CONNECTED if event.header.node == listener_key => {
                                first_connection = String::from_utf8_lossy(&event.data).into_owned();
                            }
                            LP2P_EV_HOLE_PUNCH if event.header.node == listener_key => {
                                let data = String::from_utf8_lossy(&event.data).into_owned();
                                punched = Some(if event.header.reason == 0 {
                                    Ok(data)
                                } else {
                                    Err(data)
                                });
                            }
                            LP2P_EV_RPC_RESPONSE if event.header.id == id => {
                                answered = true;
                                break 'call;
                            }
                            LP2P_EV_RPC_FAILED if event.header.id == id => break 'call,
                            _ => {}
                        }
                    }
                }
                if !answered {
                    std::thread::sleep(Duration::from_secs(2));
                }
            }
            if !answered {
                say(
                    &role,
                    format!("RESULT impl=rust transport={transport} rpc=failed"),
                );
                std::process::exit(2);
            }

            // DCUtR starts on the relayed connection by itself; give it time to finish.
            let deadline = Instant::now() + Duration::from_secs(30);
            while punched.is_none() && Instant::now() < deadline {
                for event in node.poll(200) {
                    if let Some(line) = describe(&event) {
                        say(&role, line);
                    }
                    if event.header.r#type == LP2P_EV_HOLE_PUNCH && event.header.node == listener_key {
                        let data = String::from_utf8_lossy(&event.data).into_owned();
                        punched = Some(if event.header.reason == 0 {
                            Ok(data)
                        } else {
                            Err(data)
                        });
                    }
                }
            }
            let relayed_first = first_connection.contains("/p2p-circuit");
            let (outcome, code) = match &punched {
                Some(Ok(address)) => (format!("hole_punch=direct address={address}"), 0),
                Some(Err(error)) => (format!("hole_punch=failed error={error}"), 1),
                // libp2p reports a failed attempt on the side that initiated it -- the listener --
                // so the dialer often hears nothing at all; the listener's log says why.
                None => ("hole_punch=none".to_string(), 1),
            };
            say(
                &role,
                format!(
                    "RESULT impl=rust transport={transport} rpc=ok first_connection_relayed={relayed_first} {outcome}"
                ),
            );
            unsafe { lp2p_shutdown(node.handle) };
            std::process::exit(code);
        }
    }
}
