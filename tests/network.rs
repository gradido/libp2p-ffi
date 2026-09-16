//! Nodes on loopback, driven through the C interface exactly as a C caller would drive them.

use std::ffi::CString;
use std::os::raw::c_char;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use libp2p_ffi::abi::*;
use libp2p_ffi::ffi::*;

const RPC_PROTOCOLS: [&str; 2] = ["/test/echo/1", "/test/other/1"];

struct Event {
    header: lp2p_event,
    data: Vec<u8>,
}

fn parse(buf: &[u8]) -> Vec<Event> {
    let mut events = Vec::new();
    let mut offset = 0;
    while offset + LP2P_EVENT_HEADER_BYTES <= buf.len() {
        // SAFETY: the record starts with an lp2p_event; read_unaligned copes with the alignment.
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

struct TestNode {
    handle: *mut lp2p,
    key: lp2p_key,
    group: lp2p_key,
    /// Events a wait_for saw but was not waiting for; the next wait_for looks at them first.
    backlog: std::cell::RefCell<std::collections::VecDeque<Event>>,
}

// The handle is thread-safe by contract; the test hands it to a responder thread.
unsafe impl Send for TestNode {}

fn key_of_seed(seed: [u8; 32]) -> lp2p_key {
    let mut key = [0u8; 32];
    assert_eq!(
        unsafe { lp2p_key_from_seed(seed.as_ptr(), key.as_mut_ptr()) },
        LP2P_OK
    );
    key
}

/// How a test node differs from the defaults.
#[derive(Clone, Copy)]
struct Setup {
    reachability: u8,
    dcutr: u8,
    announce: u8,
    /// Listen on a loopback port; a node that does not can only dial out.
    listen: bool,
}

impl Default for Setup {
    fn default() -> Self {
        Setup {
            reachability: LP2P_REACH_UNKNOWN,
            dcutr: 1,
            announce: 1,
            listen: true,
        }
    }
}

fn start(node_seed: [u8; 32], group_seed: [u8; 32]) -> Result<TestNode, i32> {
    start_with(node_seed, group_seed, node_seed, Setup::default())
}

fn start_as(node_seed: [u8; 32], group_seed: [u8; 32], setup: Setup) -> Result<TestNode, i32> {
    start_with(node_seed, group_seed, node_seed, setup)
}

/// @p delegated_seed is the node the delegation names; a different one makes it invalid.
fn start_with(
    node_seed: [u8; 32],
    group_seed: [u8; 32],
    delegated_seed: [u8; 32],
    setup: Setup,
) -> Result<TestNode, i32> {
    let key = key_of_seed(node_seed);
    let group = key_of_seed(group_seed);
    let mut delegation = [0u8; LP2P_DELEGATION_BYTES];
    let delegated = key_of_seed(delegated_seed);
    assert_eq!(
        unsafe {
            lp2p_delegation_sign(
                group_seed.as_ptr(),
                delegated.as_ptr(),
                0,
                delegation.as_mut_ptr(),
            )
        },
        LP2P_OK
    );

    let listen = CString::new("/ip4/127.0.0.1/tcp/0").unwrap();
    let listen_list = [listen.as_ptr()];
    let dht = CString::new("/test/kad/1").unwrap();
    let protocols: Vec<CString> = RPC_PROTOCOLS.iter().map(|p| CString::new(*p).unwrap()).collect();
    let protocol_list: Vec<*const c_char> = protocols.iter().map(|p| p.as_ptr()).collect();

    let mut options = std::mem::MaybeUninit::<lp2p_options>::uninit();
    unsafe { lp2p_options_default(options.as_mut_ptr()) };
    let mut options = unsafe { options.assume_init() };
    options.node_seed = node_seed;
    options.delegation = delegation.as_ptr();
    options.delegation_len = delegation.len();
    options.group = group;
    options.listen_addrs = listen_list.as_ptr();
    options.listen_addr_count = usize::from(setup.listen);
    options.dht_protocol = dht.as_ptr();
    options.rpc_protocols = protocol_list.as_ptr();
    options.rpc_protocol_count = protocol_list.len();
    options.quic = 0;
    options.rpc_timeout_ms = 3000;
    options.reachability = setup.reachability;
    options.dcutr = setup.dcutr;
    options.announce.enabled = setup.announce;

    let mut handle = std::ptr::null_mut();
    match unsafe { lp2p_start(&options, &mut handle) } {
        LP2P_OK => Ok(TestNode {
            handle,
            key,
            group,
            backlog: Default::default(),
        }),
        status => Err(status),
    }
}

impl TestNode {
    fn poll(&self, timeout_ms: i32) -> Vec<Event> {
        let mut buf = vec![0u8; 1 << 16];
        let n = unsafe { lp2p_poll(self.handle, buf.as_mut_ptr(), buf.len(), timeout_ms) };
        assert!(n >= 0, "poll answered {n}");
        parse(&buf[..n as usize])
    }

    fn wait_for(&self, timeout: Duration, mut pred: impl FnMut(&Event) -> bool) -> Event {
        let mut backlog = self.backlog.borrow_mut();
        if let Some(position) = backlog.iter().position(&mut pred) {
            return backlog.remove(position).unwrap();
        }
        let deadline = Instant::now() + timeout;
        let mut found = None;
        while found.is_none() && Instant::now() < deadline {
            // The whole batch goes through: what follows the match in it stays for later.
            for event in self.poll(100) {
                if found.is_none() && pred(&event) {
                    found = Some(event);
                } else {
                    backlog.push_back(event);
                }
            }
        }
        if let Some(event) = found {
            return event;
        }
        let seen: Vec<String> = backlog
            .iter()
            .map(|e| {
                format!(
                    "{}:{}:{}",
                    e.header.r#type,
                    e.header.reason,
                    String::from_utf8_lossy(&e.data)
                )
            })
            .collect();
        panic!("event did not arrive within {timeout:?}; unmatched events: {seen:?}");
    }

    /// Like wait_for, but answers None instead of failing when nothing matching arrives.
    fn wait_until(&self, timeout: Duration, mut pred: impl FnMut(&Event) -> bool) -> Option<Event> {
        let deadline = Instant::now() + timeout;
        let mut found = None;
        while found.is_none() && Instant::now() < deadline {
            for event in self.poll(100) {
                if found.is_none() && pred(&event) {
                    found = Some(event);
                } else {
                    self.backlog.borrow_mut().push_back(event);
                }
            }
        }
        found
    }

    fn listen_address(&self) -> String {
        let event = self.wait_for(Duration::from_secs(5), |e| e.header.r#type == LP2P_EV_LISTENING);
        String::from_utf8(event.data).unwrap()
    }

    fn add_address(&self, other: &TestNode, address: &str) {
        let address = CString::new(address).unwrap();
        assert_eq!(
            unsafe { lp2p_add_address(self.handle, other.key.as_ptr(), address.as_ptr()) },
            LP2P_OK
        );
    }

    fn bootstrap(&self) {
        let mut id = 0;
        assert_eq!(unsafe { lp2p_dht_bootstrap(self.handle, &mut id) }, LP2P_OK);
        self.wait_for(Duration::from_secs(10), |e| {
            e.header.r#type == LP2P_EV_DHT_RESULT && e.header.id == id
        });
    }

    fn request(&self, group: &lp2p_key, node: Option<&lp2p_key>, protocol: u16, payload: &[u8]) -> u64 {
        let mut id = 0;
        let status = unsafe {
            lp2p_rpc_request(
                self.handle,
                group.as_ptr(),
                node.map_or(std::ptr::null(), |k| k.as_ptr()),
                protocol,
                payload.as_ptr(),
                payload.len(),
                0,
                &mut id,
            )
        };
        assert_eq!(status, LP2P_OK);
        id
    }

    fn outcome(&self, id: u64) -> Event {
        self.wait_for(Duration::from_secs(20), |e| {
            (e.header.r#type == LP2P_EV_RPC_RESPONSE || e.header.r#type == LP2P_EV_RPC_FAILED)
                && e.header.id == id
        })
    }

    fn shutdown(self) {
        assert_eq!(unsafe { lp2p_shutdown(self.handle) }, LP2P_OK);
    }
}

/// What a responder hands back when it stops: the node, and the (group, protocol) of every request.
type Responder = thread::JoinHandle<(TestNode, Vec<(lp2p_key, u16)>)>;

/// Answers every request with "<prefix>:<payload>" until told to stop, and reports what it saw.
fn responder(node: TestNode, prefix: &'static str) -> (mpsc::Sender<()>, Responder) {
    let (stop, stopped) = mpsc::channel();
    let thread = thread::spawn(move || {
        let mut seen = Vec::new();
        while stopped.try_recv().is_err() {
            for event in node.poll(50) {
                if event.header.r#type == LP2P_EV_RPC_REQUEST {
                    seen.push((event.header.group, event.header.protocol));
                    let mut answer = prefix.as_bytes().to_vec();
                    answer.push(b':');
                    answer.extend_from_slice(&event.data);
                    unsafe { lp2p_rpc_respond(node.handle, event.header.id, answer.as_ptr(), answer.len()) };
                }
            }
        }
        (node, seen)
    });
    (stop, thread)
}

#[test]
fn a_group_is_called_by_its_key_and_fails_over_to_the_next_node() {
    let group_a = [0xa0; 32];
    let group_b = [0xb0; 32];
    let a1 = start([1; 32], group_a).unwrap();
    let a2 = start([2; 32], group_a).unwrap();
    let b = start([3; 32], group_b).unwrap();

    let a1_address = a1.listen_address();
    let _ = a2.listen_address();
    let _ = b.listen_address();
    a2.add_address(&a1, &a1_address);
    b.add_address(&a1, &a1_address);
    a2.bootstrap();
    b.bootstrap();

    let (a1_key, a2_key, group_a_key) = (a1.key, a2.key, a1.group);
    let (stop1, t1) = responder(a1, "a1");
    let (stop2, t2) = responder(a2, "a2");
    // Provider records are published with a short debounce after the first peers arrive.
    thread::sleep(Duration::from_secs(3));

    // By group: whichever node answers, it has to be one of group A's, and say so.
    let id = b.request(&group_a_key, None, 0, b"ping");
    let answer = b.outcome(id);
    assert_eq!(
        answer.header.r#type, LP2P_EV_RPC_RESPONSE,
        "reason {}",
        answer.header.reason
    );
    assert_eq!(answer.header.group, group_a_key);
    assert!(answer.header.node == a1_key || answer.header.node == a2_key);
    assert!(
        answer.data == b"a1:ping" || answer.data == b"a2:ping",
        "{:?}",
        answer.data
    );

    // Pinned to one node.
    let id = b.request(&group_a_key, Some(&a2_key), 1, b"pinned");
    let answer = b.outcome(id);
    assert_eq!(answer.header.r#type, LP2P_EV_RPC_RESPONSE);
    assert_eq!(answer.data, b"a2:pinned");
    assert_eq!(answer.header.node, a2_key);

    // Failover: a2 answered last and goes away. The next call asks it first, fails over to a1,
    // and from then on a1 is asked first -- every call gets an answer, and the dead node is asked
    // exactly once.
    stop2.send(()).unwrap();
    let (a2, seen2) = t2.join().unwrap();
    a2.shutdown();
    for round in 0..5 {
        let id = b.request(&group_a_key, None, 0, format!("after{round}").as_bytes());
        let answer = b.outcome(id);
        assert_eq!(
            answer.header.r#type, LP2P_EV_RPC_RESPONSE,
            "round {round}, reason {}",
            answer.header.reason
        );
        assert_eq!(answer.header.node, a1_key);
    }
    let mut stats = lp2p_stats {
        size: std::mem::size_of::<lp2p_stats>() as u32,
        ..Default::default()
    };
    assert_eq!(unsafe { lp2p_stats_get(b.handle, &mut stats) }, LP2P_OK);
    assert_eq!(stats.rpc_out, 7 + 1, "seven calls, one of them failed over once");

    // A group nobody belongs to fails, and says why.
    let id = b.request(&[0x77; 32], None, 0, b"nobody");
    let answer = b.outcome(id);
    assert_eq!(answer.header.r#type, LP2P_EV_RPC_FAILED);
    assert_eq!(answer.header.reason, LP2P_FAIL_UNREACHABLE);

    stop1.send(()).unwrap();
    let (a1, seen1) = t1.join().unwrap();
    // Every request a1 or a2 saw came from group B, on a protocol the caller named.
    for (group, protocol) in seen1.iter().chain(&seen2) {
        assert_eq!(*group, b.group);
        assert!(*protocol < 2);
    }
    a1.shutdown();
    b.shutdown();
}

#[test]
fn a_node_with_someone_elses_delegation_does_not_start() {
    assert_eq!(
        start_with([4; 32], [0xc0; 32], [5; 32], Setup::default()).err(),
        Some(LP2P_ERR_INVALID_ARGUMENT)
    );
}

#[test]
fn a_shut_down_node_leaves_no_thread_waiting() {
    let node = start([6; 32], [0xd0; 32]).unwrap();
    let _ = node.listen_address();
    let mut id = 0;
    let payload = b"x";
    // An unknown protocol index is refused before anything is sent.
    let status = unsafe {
        lp2p_rpc_request(
            node.handle,
            node.group.as_ptr(),
            std::ptr::null(),
            9,
            payload.as_ptr(),
            1,
            0,
            &mut id,
        )
    };
    assert_eq!(status, LP2P_ERR_INVALID_ARGUMENT);
    let mut stats = lp2p_stats {
        size: std::mem::size_of::<lp2p_stats>() as u32,
        ..Default::default()
    };
    assert_eq!(unsafe { lp2p_stats_get(node.handle, &mut stats) }, LP2P_OK);
    let started = Instant::now();
    node.shutdown();
    assert!(started.elapsed() < Duration::from_secs(5));
}

/// A private node announces only relayed addresses, so a call that reaches it went through the
/// relay -- at least until DCUtR has upgraded the connection, which the second variant allows.
fn a_private_node_is_called_through_a_relay(dcutr: u8, seed: u8) {
    let public = Setup {
        reachability: LP2P_REACH_PUBLIC,
        dcutr,
        ..Setup::default()
    };
    let private = Setup {
        reachability: LP2P_REACH_PRIVATE,
        dcutr,
        ..Setup::default()
    };
    let relay = start_as([seed; 32], [seed.wrapping_add(0x80); 32], public).unwrap();
    let relay_address = relay.listen_address();

    let hidden = start_as([seed + 1; 32], [seed.wrapping_add(0x81); 32], private).unwrap();
    hidden.add_address(&relay, &relay_address);
    hidden.bootstrap();
    let circuit = hidden.wait_for(Duration::from_secs(10), |e| {
        e.header.r#type == LP2P_EV_LISTENING && String::from_utf8_lossy(&e.data).contains("/p2p-circuit")
    });
    assert!(String::from_utf8_lossy(&circuit.data).contains(&relay_address));

    let caller = start_as([seed + 2; 32], [seed.wrapping_add(0x82); 32], public).unwrap();
    caller.add_address(&relay, &relay_address);
    caller.bootstrap();

    let (hidden_key, hidden_group) = (hidden.key, hidden.group);
    let (stop, responding) = responder(hidden, "hidden");
    // The provider record is re-published with the relayed address after the reservation.
    thread::sleep(Duration::from_secs(3));

    for round in 0..3 {
        let id = caller.request(&hidden_group, None, 0, format!("through{round}").as_bytes());
        let answer = caller.outcome(id);
        assert_eq!(
            answer.header.r#type, LP2P_EV_RPC_RESPONSE,
            "round {round}, reason {}",
            answer.header.reason
        );
        assert_eq!(answer.header.node, hidden_key);
        assert_eq!(answer.data, format!("hidden:through{round}").as_bytes());
    }

    // The caller's first connection to the hidden node went through the relay.
    let connected = caller.wait_for(Duration::from_secs(1), |e| {
        e.header.r#type == LP2P_EV_PEER_CONNECTED && e.header.node == hidden_key
    });
    let address = String::from_utf8_lossy(&connected.data).into_owned();
    assert!(address.contains("/p2p-circuit"), "{address}");

    stop.send(()).unwrap();
    let (hidden, _) = responding.join().unwrap();
    hidden.shutdown();
    caller.shutdown();
    relay.shutdown();
}

#[test]
fn a_private_node_is_called_through_a_relay_alone() {
    a_private_node_is_called_through_a_relay(0, 40);
}

#[test]
fn a_private_node_is_called_through_a_relay_with_hole_punching() {
    a_private_node_is_called_through_a_relay(1, 50);
}

/// Calls @p server from @p caller, pinned, answering on the server side in between. Answers the
/// caller's outcome and every LP2P_EV_LIMITED the server reported meanwhile.
fn call_and_serve(caller: &TestNode, server: &TestNode, payload: &[u8]) -> (Event, Vec<Event>) {
    let id = caller.request(&server.group, Some(&server.key), 0, payload);
    let mut limited = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        for event in server.poll(20) {
            match event.header.r#type {
                LP2P_EV_RPC_REQUEST => unsafe {
                    lp2p_rpc_respond(server.handle, event.header.id, b"ok".as_ptr(), 2);
                },
                LP2P_EV_LIMITED => limited.push(event),
                _ => {}
            }
        }
        for event in caller.poll(20) {
            let kind = event.header.r#type;
            if (kind == LP2P_EV_RPC_RESPONSE || kind == LP2P_EV_RPC_FAILED) && event.header.id == id {
                return (event, limited);
            }
        }
    }
    panic!("no outcome for call {id}");
}

#[test]
fn limits_apply_per_class_and_a_blocked_group_is_refused() {
    let server = start([60; 32], [0xf0; 32]).unwrap();
    let caller = start([61; 32], [0xf1; 32]).unwrap();
    let address = server.listen_address();
    caller.add_address(&server, &address);

    // Two calls per minute per node, for groups nobody classified.
    let per_minute = lp2p_rate {
        amount: 1,
        interval_ms: 60_000,
        burst: 2,
    };
    assert_eq!(
        unsafe {
            lp2p_limit_set(
                server.handle,
                LP2P_CLASS_UNKNOWN,
                LP2P_SCOPE_PEER,
                LP2P_PROTOCOL_ANY,
                per_minute,
            )
        },
        LP2P_OK
    );
    for round in 0..2 {
        let (outcome, limited) = call_and_serve(&caller, &server, b"within");
        assert_eq!(outcome.header.r#type, LP2P_EV_RPC_RESPONSE, "round {round}");
        assert!(limited.is_empty());
    }
    let (outcome, limited) = call_and_serve(&caller, &server, b"over");
    assert_eq!(outcome.header.r#type, LP2P_EV_RPC_FAILED);
    assert_eq!(outcome.header.reason, LP2P_FAIL_REFUSED);
    assert_eq!(limited.len(), 1);
    assert_eq!(limited[0].header.reason, LP2P_SCOPE_PEER as u16);
    assert_eq!(limited[0].header.node, caller.key);
    assert_eq!(limited[0].header.group, caller.group);

    // The caller's group in a class of its own, which has no limit: it gets through again.
    assert_eq!(
        unsafe { lp2p_peer_set_class(server.handle, caller.group.as_ptr(), 7) },
        LP2P_OK
    );
    let (outcome, _) = call_and_serve(&caller, &server, b"classified");
    assert_eq!(outcome.header.r#type, LP2P_EV_RPC_RESPONSE);

    // Blocked: refused whatever the limits say.
    assert_eq!(
        unsafe { lp2p_peer_set_class(server.handle, caller.group.as_ptr(), LP2P_CLASS_BLOCKED) },
        LP2P_OK
    );
    let (outcome, limited) = call_and_serve(&caller, &server, b"blocked");
    assert_eq!(outcome.header.r#type, LP2P_EV_RPC_FAILED);
    assert_eq!(limited.len(), 1);
    assert_eq!(limited[0].header.reason, LP2P_LIMITED_BLOCKED);

    // A limit for the blocked class, or for a protocol the node does not have, is refused.
    assert_eq!(
        unsafe {
            lp2p_limit_set(
                server.handle,
                LP2P_CLASS_BLOCKED,
                LP2P_SCOPE_PEER,
                LP2P_PROTOCOL_ANY,
                per_minute,
            )
        },
        LP2P_ERR_INVALID_ARGUMENT
    );
    assert_eq!(
        unsafe { lp2p_limit_set(server.handle, 7, LP2P_SCOPE_PEER, 9, per_minute) },
        LP2P_ERR_INVALID_ARGUMENT
    );

    let mut stats = lp2p_stats {
        size: std::mem::size_of::<lp2p_stats>() as u32,
        ..Default::default()
    };
    assert_eq!(unsafe { lp2p_stats_get(server.handle, &mut stats) }, LP2P_OK);
    assert_eq!(stats.rpc_limited, 2);
    server.shutdown();
    caller.shutdown();
}

impl TestNode {
    fn announce(&self, payload: &[u8]) -> i32 {
        unsafe { lp2p_announce_set_payload(self.handle, payload.as_ptr(), payload.len()) }
    }

    fn announcements(&self, within: Duration) -> Vec<Event> {
        let deadline = Instant::now() + within;
        let mut found = Vec::new();
        while Instant::now() < deadline {
            for event in self.poll(50) {
                if event.header.r#type == LP2P_EV_ANNOUNCEMENT {
                    found.push(event);
                }
            }
        }
        found
    }
}

#[test]
fn an_announcement_reaches_every_subscribed_node_once_per_change() {
    let hub = start([70; 32], [0x70; 32]).unwrap();
    let speaker = start([71; 32], [0x71; 32]).unwrap();
    let listener = start([72; 32], [0x72; 32]).unwrap();
    let hub_address = hub.listen_address();
    for node in [&speaker, &listener] {
        node.add_address(&hub, &hub_address);
        node.bootstrap();
    }
    // Subscriptions travel with the connections; the mesh forms on gossipsub's one-second heartbeat.
    thread::sleep(Duration::from_secs(2));

    assert_eq!(speaker.announce(b"api 1, https://speaker.example"), LP2P_OK);
    let heard = listener.wait_for(Duration::from_secs(10), |e| {
        e.header.r#type == LP2P_EV_ANNOUNCEMENT
    });
    assert_eq!(heard.header.node, speaker.key);
    assert_eq!(heard.header.group, speaker.group);
    assert_eq!(heard.data, b"api 1, https://speaker.example");
    // The hub heard it too, and nobody hears their own.
    hub.wait_for(Duration::from_secs(5), |e| {
        e.header.r#type == LP2P_EV_ANNOUNCEMENT && e.header.node == speaker.key
    });
    assert!(speaker.announcements(Duration::from_millis(300)).is_empty());

    // The same payload again is not announced again; a changed one is.
    assert_eq!(speaker.announce(b"api 1, https://speaker.example"), LP2P_OK);
    assert!(listener.announcements(Duration::from_secs(2)).is_empty());
    assert_eq!(speaker.announce(b"api 2"), LP2P_OK);
    let heard = listener.wait_for(Duration::from_secs(10), |e| {
        e.header.r#type == LP2P_EV_ANNOUNCEMENT
    });
    assert_eq!(heard.data, b"api 2");

    // A listener that blocked the speaker's group does not hear it any more; the hub still does.
    assert_eq!(
        unsafe { lp2p_peer_set_class(listener.handle, speaker.group.as_ptr(), LP2P_CLASS_BLOCKED) },
        LP2P_OK
    );
    assert_eq!(speaker.announce(b"api 3"), LP2P_OK);
    hub.wait_for(Duration::from_secs(10), |e| {
        e.header.r#type == LP2P_EV_ANNOUNCEMENT && e.data == b"api 3"
    });
    assert!(listener.announcements(Duration::from_secs(2)).is_empty());

    // The speaker has used three announcements within seconds; the fourth is not passed on.
    assert_eq!(speaker.announce(b"api 4"), LP2P_OK);
    assert!(
        hub.announcements(Duration::from_secs(2)).is_empty(),
        "the hub reported a fourth announcement within ten seconds"
    );

    // Too large is refused; with announcements off there is nothing to set.
    assert_eq!(speaker.announce(&[0u8; 1025]), LP2P_ERR_INVALID_ARGUMENT);
    let quiet = start_as(
        [73; 32],
        [0x73; 32],
        Setup {
            announce: 0,
            ..Setup::default()
        },
    )
    .unwrap();
    assert_eq!(quiet.announce(b"x"), LP2P_ERR_UNAVAILABLE);
    quiet.shutdown();
    hub.shutdown();
    speaker.shutdown();
    listener.shutdown();
}

/// AutoNAT decides for a node configured UNKNOWN: one that can be dialed back becomes PUBLIC, one
/// that cannot becomes PRIVATE and reserves on a relay. On loopback only with the test-loopback
/// feature, which lets AutoNAT accept loopback addresses and probe within seconds:
/// `cargo test --features test-loopback`.
#[test]
#[cfg_attr(not(feature = "test-loopback"), ignore = "needs --features test-loopback")]
fn autonat_decides_for_a_node_configured_unknown() {
    let reach = |node: &TestNode, expected: u8| {
        node.wait_for(Duration::from_secs(30), |e| {
            e.header.r#type == LP2P_EV_REACHABILITY && e.header.reason == u16::from(expected)
        });
    };
    let server = start_as(
        [80; 32],
        [0x90; 32],
        Setup {
            reachability: LP2P_REACH_PUBLIC,
            ..Setup::default()
        },
    )
    .unwrap();
    let address = server.listen_address();

    let reachable = start_as([81; 32], [0x91; 32], Setup::default()).unwrap();
    let dial_only = start_as(
        [82; 32],
        [0x92; 32],
        Setup {
            listen: false,
            ..Setup::default()
        },
    )
    .unwrap();
    for node in [&reachable, &dial_only] {
        reach(node, LP2P_REACH_UNKNOWN);
        node.add_address(&server, &address);
        node.bootstrap();
    }

    reach(&reachable, LP2P_REACH_PUBLIC);
    reach(&dial_only, LP2P_REACH_PRIVATE);
    let circuit = dial_only.wait_for(Duration::from_secs(10), |e| {
        e.header.r#type == LP2P_EV_LISTENING && String::from_utf8_lossy(&e.data).contains("/p2p-circuit")
    });
    // Reserved on a relay among its peers: the server, or the node AutoNAT found public, which
    // relays as well.
    assert!(String::from_utf8_lossy(&circuit.data).starts_with("/ip4/127.0.0.1/tcp/"));

    // A configured node keeps what it was told: the server reports its reachability once.
    reach(&server, LP2P_REACH_PUBLIC);
    let later = server.wait_until(Duration::from_secs(3), |e| {
        e.header.r#type == LP2P_EV_REACHABILITY
    });
    assert!(later.is_none(), "a PUBLIC node changed its reachability");

    for node in [server, reachable, dial_only] {
        node.shutdown();
    }
}

impl TestNode {
    fn subscribe(&self, topic: &lp2p_key) -> i32 {
        unsafe { lp2p_topic_subscribe(self.handle, topic.as_ptr()) }
    }

    fn unsubscribe(&self, topic: &lp2p_key) -> i32 {
        unsafe { lp2p_topic_unsubscribe(self.handle, topic.as_ptr()) }
    }

    fn publish(&self, topic: &lp2p_key, payload: &[u8]) -> i32 {
        unsafe { lp2p_topic_publish(self.handle, topic.as_ptr(), payload.as_ptr(), payload.len()) }
    }

    fn topic_peers(&self, topic: &lp2p_key) -> i32 {
        unsafe { lp2p_topic_peers(self.handle, topic.as_ptr()) }
    }

    /// The topic messages that arrive within @p within.
    fn messages(&self, within: Duration) -> Vec<Event> {
        let deadline = Instant::now() + within;
        let mut out = Vec::new();
        while Instant::now() < deadline {
            for event in self.poll(100) {
                if event.header.r#type == LP2P_EV_TOPIC_MESSAGE {
                    out.push(event);
                }
            }
        }
        out
    }

    fn stats(&self) -> lp2p_stats {
        let mut stats = lp2p_stats {
            size: std::mem::size_of::<lp2p_stats>() as u32,
            ..Default::default()
        };
        assert_eq!(unsafe { lp2p_stats_get(self.handle, &mut stats) }, LP2P_OK);
        stats
    }

    /// Waits until the topic has a mesh peer, so that a publication reaches somebody.
    fn await_topic_peers(&self, topic: &lp2p_key, timeout: Duration) -> i32 {
        let deadline = Instant::now() + timeout;
        loop {
            let peers = self.topic_peers(topic);
            if peers > 0 || Instant::now() >= deadline {
                return peers;
            }
            // Polling keeps the queue from filling while the mesh forms.
            let _ = self.poll(200);
        }
    }
}

/// A topic key is 32 bytes of the caller's choosing; these two stand for two shards.
const TOPIC: lp2p_key = [0x5a; 32];
const OTHER_TOPIC: lp2p_key = [0x5b; 32];

#[test]
fn a_topic_carries_messages_between_nodes_that_cannot_dial_each_other() {
    // Neither sender listens, so nothing they say can travel on a connection between them: what
    // arrives went through the hub, which forwards because it follows the topic too.
    let hub = start([80; 32], [0x80; 32]).unwrap();
    let a = start_as(
        [81; 32],
        [0x81; 32],
        Setup {
            listen: false,
            ..Setup::default()
        },
    )
    .unwrap();
    let b = start_as(
        [82; 32],
        [0x82; 32],
        Setup {
            listen: false,
            ..Setup::default()
        },
    )
    .unwrap();
    let hub_address = hub.listen_address();
    for node in [&a, &b] {
        node.add_address(&hub, &hub_address);
        node.bootstrap();
    }
    for node in [&hub, &a, &b] {
        assert_eq!(node.subscribe(&TOPIC), LP2P_OK);
    }
    assert!(a.await_topic_peers(&TOPIC, Duration::from_secs(15)) > 0);

    assert_eq!(a.publish(&TOPIC, b"block 1"), LP2P_OK);
    let heard = b.wait_for(Duration::from_secs(15), |e| {
        e.header.r#type == LP2P_EV_TOPIC_MESSAGE
    });
    assert_eq!(&heard.data[..32], &TOPIC[..], "the topic key comes first");
    assert_eq!(&heard.data[32..], b"block 1");
    assert_eq!(heard.header.node, a.key);
    assert_eq!(heard.header.group, a.group);
    // The hub follows the topic as well, so it both reported and forwarded the message.
    hub.wait_for(Duration::from_secs(5), |e| {
        e.header.r#type == LP2P_EV_TOPIC_MESSAGE && e.data[32..] == *b"block 1"
    });
    assert!(a.messages(Duration::from_millis(300)).is_empty(), "heard itself");

    // A topic nobody subscribed to reaches nobody, and neither does one that was left.
    assert_eq!(a.publish(&OTHER_TOPIC, b"nowhere"), LP2P_OK);
    assert!(b.messages(Duration::from_secs(2)).is_empty());
    assert_eq!(b.unsubscribe(&TOPIC), LP2P_OK);
    thread::sleep(Duration::from_secs(2));
    assert_eq!(a.publish(&TOPIC, b"block 2"), LP2P_OK);
    assert!(b.messages(Duration::from_secs(3)).is_empty());
    hub.wait_for(Duration::from_secs(10), |e| {
        e.header.r#type == LP2P_EV_TOPIC_MESSAGE && e.data[32..] == *b"block 2"
    });

    let stats = a.stats();
    assert_eq!(stats.topics_subscribed, 1);
    // Two publications went out; the one into the topic a does not follow was dropped.
    assert_eq!(stats.topic_out, 2);
    assert!(stats.topic_bytes_out > 2 * LP2P_DELEGATION_BYTES as u64);
    assert_eq!(b.stats().topics_subscribed, 0);
    assert!(hub.stats().topic_in >= 2);

    // A message above the caller's bound is refused where it is published.
    assert_eq!(
        a.publish(&TOPIC, &[0u8; (64 << 10) + 1]),
        LP2P_ERR_INVALID_ARGUMENT
    );

    hub.shutdown();
    a.shutdown();
    b.shutdown();
}

#[test]
fn members_of_a_topic_find_each_other_through_the_dht() {
    // The two members never hear of each other from anyone: the bootstrap node does not follow
    // the topic, so it neither forwards the message nor is part of the mesh. They only have the
    // provider records under the topic key to go by.
    let seed_node = start([85; 32], [0x85; 32]).unwrap();
    let a = start([86; 32], [0x86; 32]).unwrap();
    let b = start([87; 32], [0x87; 32]).unwrap();
    let seed_address = seed_node.listen_address();
    let _ = a.listen_address();
    let _ = b.listen_address();
    for node in [&a, &b] {
        node.add_address(&seed_node, &seed_address);
        node.bootstrap();
    }
    assert_eq!(a.subscribe(&TOPIC), LP2P_OK);
    // Far enough apart that a's provider record is in the DHT before b looks it up, and close
    // enough that b's first lookup, not its repeat, is what finds it.
    thread::sleep(Duration::from_secs(2));
    assert_eq!(b.subscribe(&TOPIC), LP2P_OK);

    assert!(
        b.await_topic_peers(&TOPIC, Duration::from_secs(30)) > 0,
        "the topic lookup did not bring the two members together"
    );
    assert_eq!(b.publish(&TOPIC, b"found you"), LP2P_OK);
    let heard = a.wait_for(Duration::from_secs(15), |e| {
        e.header.r#type == LP2P_EV_TOPIC_MESSAGE
    });
    assert_eq!(&heard.data[32..], b"found you");
    assert!(
        seed_node.messages(Duration::from_millis(300)).is_empty(),
        "a node that does not follow the topic reported a message from it"
    );

    seed_node.shutdown();
    a.shutdown();
    b.shutdown();
}

#[test]
fn a_byte_limit_stops_topic_traffic_a_node_cannot_carry() {
    let hub = start([90; 32], [0x90; 32]).unwrap();
    let speaker = start([91; 32], [0x91; 32]).unwrap();
    let hub_address = hub.listen_address();
    speaker.add_address(&hub, &hub_address);
    speaker.bootstrap();
    for node in [&hub, &speaker] {
        assert_eq!(node.subscribe(&TOPIC), LP2P_OK);
    }
    assert!(speaker.await_topic_peers(&TOPIC, Duration::from_secs(15)) > 0);

    // A frame of a 256-byte payload is 393 bytes on the wire: the version byte and the 136-byte
    // delegation on top. 400 a second with 500 in reserve lets one through and stops the next.
    assert_eq!(
        unsafe {
            lp2p_limit_set_bytes(
                hub.handle,
                LP2P_CLASS_UNKNOWN,
                LP2P_SCOPE_PEER,
                LP2P_PROTOCOL_TOPICS,
                lp2p_rate {
                    amount: 400,
                    interval_ms: 1000,
                    burst: 500,
                },
            )
        },
        LP2P_OK
    );
    assert_eq!(speaker.publish(&TOPIC, &[1u8; 256]), LP2P_OK);
    hub.wait_for(Duration::from_secs(10), |e| {
        e.header.r#type == LP2P_EV_TOPIC_MESSAGE
    });
    assert_eq!(speaker.publish(&TOPIC, &[2u8; 256]), LP2P_OK);
    let limited = hub.wait_for(Duration::from_secs(10), |e| e.header.r#type == LP2P_EV_LIMITED);
    assert_eq!(limited.header.protocol, LP2P_PROTOCOL_TOPICS);
    assert_eq!(limited.header.reason, LP2P_SCOPE_PEER as u16);
    assert_eq!(limited.header.group, speaker.group);
    assert!(hub.messages(Duration::from_secs(1)).is_empty());
    let stats = hub.stats();
    assert_eq!(stats.topic_in, 1);
    assert_eq!(stats.topic_limited, 1);

    // Room again after the bucket refilled.
    thread::sleep(Duration::from_secs(2));
    assert_eq!(speaker.publish(&TOPIC, &[3u8; 256]), LP2P_OK);
    hub.wait_for(Duration::from_secs(10), |e| {
        e.header.r#type == LP2P_EV_TOPIC_MESSAGE && e.data[32] == 3
    });

    // A protocol index that names neither an RPC nor topics is refused.
    assert_eq!(
        unsafe {
            lp2p_limit_set_bytes(
                hub.handle,
                LP2P_CLASS_UNKNOWN,
                LP2P_SCOPE_PEER,
                RPC_PROTOCOLS.len() as u16,
                lp2p_rate {
                    amount: 1,
                    interval_ms: 1000,
                    burst: 1,
                },
            )
        },
        LP2P_ERR_INVALID_ARGUMENT
    );

    hub.shutdown();
    speaker.shutdown();
}
