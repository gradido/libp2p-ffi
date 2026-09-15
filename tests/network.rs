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
}

impl Default for Setup {
    fn default() -> Self {
        Setup {
            reachability: LP2P_REACH_UNKNOWN,
            dcutr: 1,
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
    options.listen_addr_count = 1;
    options.dht_protocol = dht.as_ptr();
    options.rpc_protocols = protocol_list.as_ptr();
    options.rpc_protocol_count = protocol_list.len();
    options.quic = 0;
    options.rpc_timeout_ms = 3000;
    options.reachability = setup.reachability;
    options.dcutr = setup.dcutr;

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
        while Instant::now() < deadline {
            for event in self.poll(100) {
                if pred(&event) {
                    return event;
                }
                backlog.push_back(event);
            }
        }
        panic!("event did not arrive within {timeout:?}");
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
    };
    let private = Setup {
        reachability: LP2P_REACH_PRIVATE,
        dcutr,
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
