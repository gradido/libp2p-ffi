//! The rust-libp2p side of `interop/js`: a node driven line by line, so a test in another language
//! can use it as its counterpart.
//!
//! Environment: `SEED` and `GROUP_SEED` (one byte each, repeated to 32), `LISTEN` (a multiaddr,
//! default `/ip4/127.0.0.1/tcp/0`), `DHT_PROTOCOL` (default `/interop/kad/1`), `REACHABILITY`
//! (`public`, `private`, `unknown`), `RELAY_SERVER` (`1` to relay).
//!
//! ```text
//! stdout  READY <peer id> <node key hex> <group key hex>
//!         LISTENING <multiaddr>
//!         CONNECTED <node hex> <address>
//!         REQUEST <id> <group hex> <node hex> <protocol index> <payload>   (answered "rust:<payload>")
//!         RESPONSE <id> <group hex> <node hex> <payload>
//!         FAILED <id> <reason>
//!         ANNOUNCEMENT <group hex> <node hex> <payload>
//!         TOPIC <topic hex> <group hex> <node hex> <payload>
//!         DHT <id> <reason>
//!         CALLED <id> | BOOTSTRAP <id> | OK | ERROR <status>
//!
//! stdin   announce <text>
//!         subscribe <topic hex> | unsubscribe <topic hex>
//!         publish <topic hex> <text>
//!         peers <topic hex>                                   (answers PEERS <count>)
//!         call <group hex> <node hex or -> <text>
//!         add <node hex> <multiaddr>
//!         bootstrap
//!         quit
//! ```

use std::ffi::CString;
use std::io::{BufRead, Write};
use std::os::raw::c_char;
use std::sync::{Arc, Mutex};

use libp2p::identity::{PublicKey, ed25519};
use libp2p_ffi::abi::*;
use libp2p_ffi::ffi::*;

const PROTOCOLS: [&str; 2] = ["/interop/echo/1", "/interop/other/1"];

struct Handle(*mut lp2p);
// The C interface is thread-safe by contract; stdin commands come from a second thread.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<lp2p_key> {
    if text.len() != 64 {
        return None;
    }
    let mut key = [0u8; 32];
    for (i, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(key)
}

fn byte_env(name: &str, default: u8) -> u8 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn key_of(seed: [u8; 32]) -> lp2p_key {
    let mut key = [0u8; 32];
    assert_eq!(
        unsafe { lp2p_key_from_seed(seed.as_ptr(), key.as_mut_ptr()) },
        LP2P_OK
    );
    key
}

fn main() {
    let out = Arc::new(Mutex::new(std::io::stdout()));
    let say = {
        let out = out.clone();
        move |line: String| {
            let mut out = out.lock().unwrap();
            let _ = writeln!(out, "{line}");
            let _ = out.flush();
        }
    };

    let seed = [byte_env("SEED", 7); 32];
    let group_seed = [byte_env("GROUP_SEED", 0x70); 32];
    let key = key_of(seed);
    let group = key_of(group_seed);
    let mut delegation = [0u8; LP2P_DELEGATION_BYTES];
    assert_eq!(
        unsafe { lp2p_delegation_sign(group_seed.as_ptr(), key.as_ptr(), 0, delegation.as_mut_ptr()) },
        LP2P_OK
    );
    let listen =
        CString::new(std::env::var("LISTEN").unwrap_or_else(|_| "/ip4/127.0.0.1/tcp/0".into())).unwrap();
    let listen_list = [listen.as_ptr()];
    let dht =
        CString::new(std::env::var("DHT_PROTOCOL").unwrap_or_else(|_| "/interop/kad/1".into())).unwrap();
    let protocols: Vec<CString> = PROTOCOLS.iter().map(|p| CString::new(*p).unwrap()).collect();
    let protocol_list: Vec<*const c_char> = protocols.iter().map(|p| p.as_ptr()).collect();

    let mut options = std::mem::MaybeUninit::<lp2p_options>::uninit();
    unsafe { lp2p_options_default(options.as_mut_ptr()) };
    let mut options = unsafe { options.assume_init() };
    options.node_seed = seed;
    options.delegation = delegation.as_ptr();
    options.delegation_len = delegation.len();
    options.group = group;
    options.listen_addrs = listen_list.as_ptr();
    options.listen_addr_count = 1;
    options.dht_protocol = dht.as_ptr();
    options.rpc_protocols = protocol_list.as_ptr();
    options.rpc_protocol_count = protocol_list.len();
    options.autonat = 0;
    options.dcutr = 0;
    options.relay.server = u8::from(std::env::var("RELAY_SERVER").as_deref() == Ok("1"));
    options.reachability = match std::env::var("REACHABILITY").as_deref() {
        Ok("private") => LP2P_REACH_PRIVATE,
        Ok("unknown") => LP2P_REACH_UNKNOWN,
        _ => LP2P_REACH_PUBLIC,
    };

    let mut raw = std::ptr::null_mut();
    let status = unsafe { lp2p_start(&options, &mut raw) };
    if status != LP2P_OK {
        say(format!("ERROR {status}"));
        std::process::exit(1);
    }
    let node = Arc::new(Handle(raw));
    let peer = PublicKey::from(ed25519::PublicKey::try_from_bytes(&key).unwrap()).to_peer_id();
    say(format!("READY {peer} {} {}", hex(&key), hex(&group)));

    {
        let node = node.clone();
        let say = say.clone();
        std::thread::spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                let mut parts = line.splitn(2, ' ');
                let command = parts.next().unwrap_or("");
                let rest = parts.next().unwrap_or("");
                match command {
                    "announce" => {
                        let status = unsafe { lp2p_announce_set_payload(node.0, rest.as_ptr(), rest.len()) };
                        say(if status == LP2P_OK {
                            "OK".into()
                        } else {
                            format!("ERROR {status}")
                        });
                    }
                    "subscribe" | "unsubscribe" => {
                        let Some(topic) = unhex(rest) else {
                            say("ERROR usage".into());
                            continue;
                        };
                        let status = unsafe {
                            if command == "subscribe" {
                                lp2p_topic_subscribe(node.0, topic.as_ptr())
                            } else {
                                lp2p_topic_unsubscribe(node.0, topic.as_ptr())
                            }
                        };
                        say(if status == LP2P_OK {
                            "OK".into()
                        } else {
                            format!("ERROR {status}")
                        });
                    }
                    "publish" => {
                        let mut args = rest.splitn(2, ' ');
                        let (Some(topic), Some(text)) = (args.next().and_then(unhex), args.next()) else {
                            say("ERROR usage".into());
                            continue;
                        };
                        let status = unsafe {
                            lp2p_topic_publish(node.0, topic.as_ptr(), text.as_ptr(), text.len())
                        };
                        say(if status == LP2P_OK {
                            "OK".into()
                        } else {
                            format!("ERROR {status}")
                        });
                    }
                    "peers" => {
                        let Some(topic) = unhex(rest) else {
                            say("ERROR usage".into());
                            continue;
                        };
                        let count = unsafe { lp2p_topic_peers(node.0, topic.as_ptr()) };
                        say(if count >= 0 {
                            format!("PEERS {count}")
                        } else {
                            format!("ERROR {count}")
                        });
                    }
                    "call" => {
                        let mut args = rest.splitn(3, ' ');
                        let (Some(group), Some(target), Some(text)) =
                            (args.next().and_then(unhex), args.next(), args.next())
                        else {
                            say("ERROR usage".into());
                            continue;
                        };
                        let target = unhex(target);
                        let mut id = 0;
                        let status = unsafe {
                            lp2p_rpc_request(
                                node.0,
                                group.as_ptr(),
                                target.as_ref().map_or(std::ptr::null(), |k| k.as_ptr()),
                                0,
                                text.as_ptr(),
                                text.len(),
                                0,
                                &mut id,
                            )
                        };
                        say(if status == LP2P_OK {
                            format!("CALLED {id}")
                        } else {
                            format!("ERROR {status}")
                        });
                    }
                    "add" => {
                        let mut args = rest.splitn(2, ' ');
                        let (Some(target), Some(address)) = (args.next().and_then(unhex), args.next()) else {
                            say("ERROR usage".into());
                            continue;
                        };
                        let address = CString::new(address).unwrap();
                        let status = unsafe { lp2p_add_address(node.0, target.as_ptr(), address.as_ptr()) };
                        say(if status == LP2P_OK {
                            "OK".into()
                        } else {
                            format!("ERROR {status}")
                        });
                    }
                    "bootstrap" => {
                        let mut id = 0;
                        let status = unsafe { lp2p_dht_bootstrap(node.0, &mut id) };
                        say(if status == LP2P_OK {
                            format!("BOOTSTRAP {id}")
                        } else {
                            format!("ERROR {status}")
                        });
                    }
                    "quit" => {
                        unsafe { lp2p_shutdown(node.0) };
                        std::process::exit(0);
                    }
                    _ => say(format!("ERROR unknown command {command}")),
                }
            }
            unsafe { lp2p_shutdown(node.0) };
            std::process::exit(0);
        });
    }

    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = unsafe { lp2p_poll(node.0, buf.as_mut_ptr(), buf.len(), 1000) };
        if n == LP2P_ERR_SHUT_DOWN {
            std::process::exit(0);
        }
        if n < 0 {
            say(format!("ERROR {n}"));
            std::process::exit(1);
        }
        let mut offset = 0usize;
        while offset + LP2P_EVENT_HEADER_BYTES <= n as usize {
            let e: lp2p_event = unsafe { std::ptr::read_unaligned(buf[offset..].as_ptr().cast()) };
            let start = offset + LP2P_EVENT_HEADER_BYTES;
            let data = &buf[start..start + e.data_len as usize];
            let text = String::from_utf8_lossy(data);
            match e.r#type {
                LP2P_EV_LISTENING => say(format!("LISTENING {text}")),
                LP2P_EV_PEER_CONNECTED => say(format!("CONNECTED {} {text}", hex(&e.node))),
                LP2P_EV_RPC_REQUEST => {
                    say(format!(
                        "REQUEST {} {} {} {} {text}",
                        e.id,
                        hex(&e.group),
                        hex(&e.node),
                        e.protocol
                    ));
                    let answer = format!("rust:{text}");
                    unsafe { lp2p_rpc_respond(node.0, e.id, answer.as_ptr(), answer.len()) };
                }
                LP2P_EV_RPC_RESPONSE => say(format!(
                    "RESPONSE {} {} {} {text}",
                    e.id,
                    hex(&e.group),
                    hex(&e.node)
                )),
                LP2P_EV_RPC_FAILED => say(format!("FAILED {} {}", e.id, e.reason)),
                LP2P_EV_ANNOUNCEMENT => {
                    say(format!("ANNOUNCEMENT {} {} {text}", hex(&e.group), hex(&e.node)))
                }
                LP2P_EV_TOPIC_MESSAGE => say(format!(
                    "TOPIC {} {} {} {}",
                    hex(&data[..32]),
                    hex(&e.group),
                    hex(&e.node),
                    String::from_utf8_lossy(&data[32..])
                )),
                LP2P_EV_DHT_RESULT => say(format!("DHT {} {}", e.id, e.reason)),
                LP2P_EV_REACHABILITY => say(format!("REACHABILITY {}", e.reason)),
                LP2P_EV_HOLE_PUNCH => say(format!("HOLE_PUNCH {} {} {text}", hex(&e.node), e.reason)),
                _ => {}
            }
            offset += e.size as usize;
        }
    }
}
