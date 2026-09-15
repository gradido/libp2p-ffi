//! The node: a swarm on a tokio runtime of its own, driven by commands and reporting events.
//!
//! Nothing here blocks the caller. Every public call turns into a [`Command`] on a channel; the
//! runtime thread applies it and pushes what follows into the event queue the caller polls.

use std::collections::{HashMap, HashSet, VecDeque};
use std::num::NonZeroU32;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use futures::StreamExt;
use libp2p::core::transport::ListenerId;
use libp2p::identity::Keypair;
use libp2p::kad::{self, store::MemoryStore};
use libp2p::multiaddr::Protocol;
use libp2p::request_response::{self, InboundRequestId, OutboundRequestId, ProtocolSupport, ResponseChannel};
use libp2p::swarm::behaviour::toggle::Toggle;
use libp2p::swarm::{NetworkBehaviour, SwarmEvent};
use libp2p::{
    Multiaddr, PeerId, StreamProtocol, Swarm, SwarmBuilder, connection_limits, dcutr, identify, noise, relay,
    tcp, yamux,
};
use tokio::sync::mpsc;

use crate::abi::*;
use crate::address_book::AddressBook;
use crate::delegation::{Delegation, now_ms};
use crate::events::{EventBuilder, EventQueue, Record};
use crate::keys;
use crate::wire::{self, RPC_PROTOCOL, RawCodec};

/// How long a provider record is re-published at most once. A burst of new peers after start
/// becomes one publication instead of one per peer.
const PROVIDE_DEBOUNCE: Duration = Duration::from_secs(2);
const TICK: Duration = Duration::from_millis(250);
/// How long a node that failed a call is asked last. A provider record outlives its node by up to
/// its TTL, and without this every call to the group would try the dead node first half the time.
const FAILURE_PENALTY: Duration = Duration::from_secs(5 * 60);
/// Bound of the per-group and per-peer memory behind the ordering above.
const MAX_REMEMBERED: usize = 4096;
/// How many relays a private node holds a reservation on. Two, so that losing one relay does not
/// make the node unreachable while it reserves on another.
const MAX_RESERVATIONS: usize = 2;
/// How long a relay that refused or dropped a reservation is left alone.
const RELAY_RETRY: Duration = Duration::from_secs(60);
const MAX_RELAY_CANDIDATES: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reachability {
    Public,
    Private,
    Unknown,
}

#[derive(Clone, Copy, Debug)]
pub struct TokenBucket {
    pub burst: NonZeroU32,
    pub interval: Duration,
}

#[derive(Clone, Copy, Debug)]
pub struct RelayConfig {
    pub server: bool,
    pub client: bool,
    pub max_reservations: usize,
    pub max_reservations_per_peer: usize,
    pub reservation_duration: Duration,
    pub max_circuits: usize,
    pub max_circuits_per_peer: usize,
    pub max_circuit_duration: Duration,
    pub max_circuit_bytes: u64,
    pub circuits_per_peer: Option<TokenBucket>,
    pub circuits_per_ip: Option<TokenBucket>,
}

pub struct Config {
    pub node_seed: [u8; 32],
    pub delegation: Delegation,
    pub listen: Vec<Multiaddr>,
    pub dht_protocol: StreamProtocol,
    pub rpc_protocols: Vec<String>,
    pub rpc_max_request_bytes: u32,
    pub rpc_max_response_bytes: u32,
    pub rpc_timeout: Duration,
    pub quic: bool,
    pub dcutr: bool,
    pub reachability: Reachability,
    pub relay: RelayConfig,
    pub max_connections: Option<u32>,
    pub max_connections_per_peer: Option<u32>,
    pub max_pending_incoming: Option<u32>,
    pub event_queue_bytes: usize,
}

pub enum Command {
    Rpc {
        id: u64,
        group: lp2p_key,
        node: Option<lp2p_key>,
        protocol: u16,
        payload: Vec<u8>,
        timeout: Duration,
    },
    Respond {
        id: u64,
        payload: Vec<u8>,
    },
    Reject {
        id: u64,
    },
    AddAddress {
        node: lp2p_key,
        address: Multiaddr,
    },
    Bootstrap {
        id: u64,
    },
    RandomWalk {
        id: u64,
    },
    RoutingSample {
        max: usize,
        reply: std::sync::mpsc::Sender<Vec<Record>>,
    },
    Shutdown,
}

/// What the caller's threads and the runtime thread both touch.
pub struct Shared {
    pub events: EventQueue,
    pub next_id: AtomicU64,
    pub connections: AtomicU32,
    pub routing_table_peers: AtomicU32,
    pub rpc_in: AtomicU64,
    pub rpc_out: AtomicU64,
    pub poisoned: AtomicBool,
}

pub struct Node {
    commands: mpsc::UnboundedSender<Command>,
    thread: Option<JoinHandle<()>>,
    pub shared: Arc<Shared>,
    pub rpc_protocol_count: usize,
    pub rpc_max_request_bytes: u32,
    pub rpc_timeout: Duration,
}

impl Node {
    pub fn start(config: Config) -> Result<Node, i32> {
        let keypair = keys::keypair_from_seed(&config.node_seed).ok_or(LP2P_ERR_INVALID_ARGUMENT)?;
        let node_key = keys::public_key(&keypair).ok_or(LP2P_ERR_INVALID_ARGUMENT)?;
        // A node that starts with a delegation nobody would accept is refused here, where the
        // caller can see why, rather than on every call it makes later.
        config
            .delegation
            .verify_for(&node_key, now_ms())
            .map_err(|_| LP2P_ERR_INVALID_ARGUMENT)?;

        let shared = Arc::new(Shared {
            events: EventQueue::new(config.event_queue_bytes),
            next_id: AtomicU64::new(1),
            connections: AtomicU32::new(0),
            routing_table_peers: AtomicU32::new(0),
            rpc_in: AtomicU64::new(0),
            rpc_out: AtomicU64::new(0),
            poisoned: AtomicBool::new(false),
        });
        let rpc_protocol_count = config.rpc_protocols.len();
        let rpc_max_request_bytes = config.rpc_max_request_bytes;
        let rpc_timeout = config.rpc_timeout;

        let (commands, receiver) = mpsc::unbounded_channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread_shared = shared.clone();
        let thread = std::thread::Builder::new()
            .name("lp2p".into())
            .spawn(move || run_thread(config, keypair, thread_shared, receiver, ready_tx))
            .map_err(|_| LP2P_ERR_NO_MEMORY)?;

        match ready_rx.recv() {
            Ok(LP2P_OK) => Ok(Node {
                commands,
                thread: Some(thread),
                shared,
                rpc_protocol_count,
                rpc_max_request_bytes,
                rpc_timeout,
            }),
            Ok(status) => {
                let _ = thread.join();
                Err(status)
            }
            Err(_) => {
                let _ = thread.join();
                Err(LP2P_ERR_PANIC)
            }
        }
    }

    pub fn next_id(&self) -> u64 {
        self.shared.next_id.fetch_add(1, Ordering::Relaxed)
    }

    pub fn send(&self, command: Command) -> i32 {
        if self.shared.poisoned.load(Ordering::Relaxed) {
            return LP2P_ERR_PANIC;
        }
        match self.commands.send(command) {
            Ok(()) => LP2P_OK,
            Err(_) => LP2P_ERR_SHUT_DOWN,
        }
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run_thread(
    config: Config,
    keypair: Keypair,
    shared: Arc<Shared>,
    commands: mpsc::UnboundedReceiver<Command>,
    ready: std::sync::mpsc::Sender<i32>,
) {
    let panic_shared = shared.clone();
    let panic_ready = ready.clone();
    let outcome = std::panic::catch_unwind(AssertUnwindSafe(move || {
        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("lp2p-worker")
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(_) => {
                let _ = ready.send(LP2P_ERR_NO_MEMORY);
                return;
            }
        };
        runtime.block_on(async move {
            let mut swarm = match build_swarm(&config, keypair) {
                Ok(swarm) => swarm,
                Err(status) => {
                    let _ = ready.send(status);
                    return;
                }
            };
            for address in &config.listen {
                if swarm.listen_on(address.clone()).is_err() {
                    let _ = ready.send(LP2P_ERR_NETWORK);
                    return;
                }
            }
            let mut state = State::new(&config, &swarm, shared);
            let reachability = match config.reachability {
                Reachability::Public => LP2P_REACH_PUBLIC,
                Reachability::Private => LP2P_REACH_PRIVATE,
                Reachability::Unknown => LP2P_REACH_UNKNOWN,
            };
            state.emit(
                EventBuilder::new(LP2P_EV_REACHABILITY)
                    .reason(u16::from(reachability))
                    .build(),
            );
            state.provide(&mut swarm);
            let _ = ready.send(LP2P_OK);
            state.run(&mut swarm, commands).await;
        });
    }));
    match outcome {
        Ok(()) => panic_shared.events.close(LP2P_ERR_SHUT_DOWN),
        Err(_) => {
            panic_shared.poisoned.store(true, Ordering::Relaxed);
            panic_shared.events.close(LP2P_ERR_PANIC);
            let _ = panic_ready.send(LP2P_ERR_PANIC);
        }
    }
}

#[derive(NetworkBehaviour)]
struct Behaviour {
    // First, so a connection over the limits is refused before any other behaviour sees it.
    limits: connection_limits::Behaviour,
    address_book: AddressBook,
    kad: kad::Behaviour<MemoryStore>,
    identify: identify::Behaviour,
    rpc: request_response::Behaviour<RawCodec>,
    // Every node can dial through a relay; only a reachable one serves as one.
    relay_client: relay::client::Behaviour,
    relay_server: Toggle<relay::Behaviour>,
    dcutr: Toggle<dcutr::Behaviour>,
}

fn relay_server_config(r: &RelayConfig) -> relay::Config {
    let mut config = relay::Config {
        max_reservations: r.max_reservations,
        max_reservations_per_peer: r.max_reservations_per_peer,
        reservation_duration: r.reservation_duration,
        max_circuits: r.max_circuits,
        max_circuits_per_peer: r.max_circuits_per_peer,
        max_circuit_duration: r.max_circuit_duration,
        max_circuit_bytes: r.max_circuit_bytes,
        ..relay::Config::default()
    };
    // The defaults carry circuit limiters of their own; the caller's replace them rather than add.
    config.circuit_src_rate_limiters = Vec::new();
    if let Some(bucket) = r.circuits_per_peer {
        config = config.circuit_src_per_peer(bucket.burst, bucket.interval);
    }
    if let Some(bucket) = r.circuits_per_ip {
        config = config.circuit_src_per_ip(bucket.burst, bucket.interval);
    }
    config
}

fn make_behaviour(config: &Config, key: &Keypair, relay_client: relay::client::Behaviour) -> Behaviour {
    let peer = key.public().to_peer_id();
    let private = config.reachability == Reachability::Private;
    let limits = connection_limits::Behaviour::new(
        connection_limits::ConnectionLimits::default()
            .with_max_established(config.max_connections)
            .with_max_established_per_peer(config.max_connections_per_peer)
            .with_max_pending_incoming(config.max_pending_incoming),
    );
    let mut kad = kad::Behaviour::with_config(
        peer,
        MemoryStore::new(peer),
        kad::Config::new(config.dht_protocol.clone()),
    );
    // A private node answers queries too, through its relays: a client-mode node would not be in
    // anyone's routing table, and its relayed addresses could not be looked up.
    kad.set_mode(Some(kad::Mode::Server));
    // A private node's own listen addresses are useless to anyone else and would be tried first;
    // it tells peers only its external addresses, which are the relayed ones.
    let identify = identify::Behaviour::new(
        identify::Config::new("/lp2p/1".to_string(), key.public()).with_hide_listen_addrs(private),
    );
    // The frame around a payload: version, delegation, and at most 256 bytes of protocol name.
    let overhead = (2 + LP2P_DELEGATION_BYTES + 255) as u64;
    let rpc = request_response::Behaviour::with_codec(
        RawCodec {
            max_request: config.rpc_max_request_bytes as u64 + overhead,
            max_response: config.rpc_max_response_bytes as u64 + overhead,
        },
        [(RPC_PROTOCOL, ProtocolSupport::Full)],
        request_response::Config::default().with_request_timeout(config.rpc_timeout),
    );
    let relay_server = (config.relay.server && !private)
        .then(|| relay::Behaviour::new(peer, relay_server_config(&config.relay)));
    let dcutr = config.dcutr.then(|| dcutr::Behaviour::new(peer));
    Behaviour {
        limits,
        address_book: AddressBook::default(),
        kad,
        identify,
        rpc,
        relay_client,
        relay_server: relay_server.into(),
        dcutr: dcutr.into(),
    }
}

fn build_swarm(config: &Config, keypair: Keypair) -> Result<Swarm<Behaviour>, i32> {
    let idle = |c: libp2p::swarm::Config| c.with_idle_connection_timeout(Duration::from_secs(60));
    let builder = SwarmBuilder::with_existing_identity(keypair)
        .with_tokio()
        .with_tcp(
            tcp::Config::default().nodelay(true),
            noise::Config::new,
            yamux::Config::default,
        )
        .map_err(|_| LP2P_ERR_NETWORK)?;
    if config.quic {
        Ok(builder
            .with_quic()
            .with_relay_client(noise::Config::new, yamux::Config::default)
            .map_err(|_| LP2P_ERR_NETWORK)?
            .with_behaviour(|key, relay| make_behaviour(config, key, relay))
            .map_err(|_| LP2P_ERR_NETWORK)?
            .with_swarm_config(idle)
            .build())
    } else {
        Ok(builder
            .with_relay_client(noise::Config::new, yamux::Config::default)
            .map_err(|_| LP2P_ERR_NETWORK)?
            .with_behaviour(|key, relay| make_behaviour(config, key, relay))
            .map_err(|_| LP2P_ERR_NETWORK)?
            .with_swarm_config(idle)
            .build())
    }
}

/// One call in progress: who is left to try, and what the last attempt said.
struct Call {
    group: lp2p_key,
    protocol: u16,
    frame: Vec<u8>,
    candidates: VecDeque<PeerId>,
    /// Every peer ever queued, so a provider reported twice is tried once.
    seen: HashSet<PeerId>,
    /// Peers whose addresses were already looked up, so a lookup that found nothing is not
    /// repeated.
    resolved: HashSet<PeerId>,
    in_flight: bool,
    resolving: bool,
    lookup_running: bool,
    deadline: Instant,
    last_failure: u16,
}

struct State {
    shared: Arc<Shared>,
    local_peer: PeerId,
    group: lp2p_key,
    delegation: [u8; LP2P_DELEGATION_BYTES],
    dht_protocol: StreamProtocol,
    rpc_protocols: Vec<String>,
    calls: HashMap<u64, Call>,
    outbound: HashMap<OutboundRequestId, u64>,
    inbound: HashMap<u64, ResponseChannel<Vec<u8>>>,
    inbound_ids: HashMap<InboundRequestId, u64>,
    provider_lookups: HashMap<kad::QueryId, u64>,
    resolutions: HashMap<kad::QueryId, u64>,
    bootstraps: HashMap<kad::QueryId, u64>,
    walks: HashMap<kad::QueryId, u64>,
    provide_due: bool,
    last_provide: Option<Instant>,
    sample_offset: usize,
    /// The node of each group that answered last; it is asked first next time.
    last_success: HashMap<lp2p_key, PeerId>,
    /// Nodes that failed a call, and when; they are asked last until the penalty is over.
    failed: HashMap<PeerId, Instant>,
    private: bool,
    relay_client: bool,
    /// Peers that offered to relay, with the address this node reached them on.
    relay_candidates: HashMap<PeerId, Multiaddr>,
    /// Relays this node holds or requested a reservation on, and the listener that stands for it.
    reservations: HashMap<PeerId, ListenerId>,
    /// Relays that refused or dropped a reservation, and when.
    relay_failed: HashMap<PeerId, Instant>,
    /// The address each connected peer was dialed on, for the peers this node dialed.
    dialed: HashMap<PeerId, Multiaddr>,
}

/// Queues @p peer for @p call: the group's last good node first, recently failed nodes last, the
/// rest in the order the DHT reported them.
fn enqueue(
    call: &mut Call,
    peer: PeerId,
    last_success: &HashMap<lp2p_key, PeerId>,
    failed: &HashMap<PeerId, Instant>,
) {
    if !call.seen.insert(peer) {
        return;
    }
    let now = Instant::now();
    let penalized = |p: &PeerId| {
        failed
            .get(p)
            .is_some_and(|t| now.duration_since(*t) < FAILURE_PENALTY)
    };
    if last_success.get(&call.group) == Some(&peer) && !penalized(&peer) {
        call.candidates.push_front(peer);
    } else if penalized(&peer) {
        call.candidates.push_back(peer);
    } else {
        let position = call
            .candidates
            .iter()
            .position(penalized)
            .unwrap_or(call.candidates.len());
        call.candidates.insert(position, peer);
    }
}

fn peer_key(peer: &PeerId) -> lp2p_key {
    keys::key_of(peer).unwrap_or([0; 32])
}

fn is_relayed(address: &Multiaddr) -> bool {
    address.iter().any(|p| matches!(p, Protocol::P2pCircuit))
}

/// @p address without a trailing /p2p/<peer>, so one can be appended.
fn without_peer(address: &Multiaddr) -> Multiaddr {
    address
        .iter()
        .filter(|p| !matches!(p, Protocol::P2p(_)))
        .collect()
}

fn is_unspecified(address: &Multiaddr) -> bool {
    match address.iter().next() {
        Some(Protocol::Ip4(ip)) => ip.is_unspecified(),
        Some(Protocol::Ip6(ip)) => ip.is_unspecified(),
        _ => false,
    }
}

impl State {
    fn new(config: &Config, swarm: &Swarm<Behaviour>, shared: Arc<Shared>) -> State {
        State {
            shared,
            local_peer: *swarm.local_peer_id(),
            group: config.delegation.group,
            delegation: config.delegation.to_bytes(),
            dht_protocol: config.dht_protocol.clone(),
            rpc_protocols: config.rpc_protocols.clone(),
            calls: HashMap::new(),
            outbound: HashMap::new(),
            inbound: HashMap::new(),
            inbound_ids: HashMap::new(),
            provider_lookups: HashMap::new(),
            resolutions: HashMap::new(),
            bootstraps: HashMap::new(),
            walks: HashMap::new(),
            provide_due: false,
            last_provide: None,
            sample_offset: 0,
            last_success: HashMap::new(),
            failed: HashMap::new(),
            private: config.reachability == Reachability::Private,
            relay_client: config.relay.client,
            relay_candidates: HashMap::new(),
            reservations: HashMap::new(),
            relay_failed: HashMap::new(),
            dialed: HashMap::new(),
        }
    }

    fn emit(&self, record: Record) {
        self.shared.events.push(record);
    }

    async fn run(&mut self, swarm: &mut Swarm<Behaviour>, mut commands: mpsc::UnboundedReceiver<Command>) {
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                command = commands.recv() => match command {
                    None | Some(Command::Shutdown) => break,
                    Some(command) => self.command(swarm, command),
                },
                event = swarm.select_next_some() => self.swarm_event(swarm, event),
                _ = tick.tick() => self.tick(swarm),
            }
        }
    }

    /// Announces this node as a provider of its group. libp2p re-publishes on its own schedule;
    /// this covers the moments that schedule misses: start, a first peer, a finished bootstrap.
    fn provide(&mut self, swarm: &mut Swarm<Behaviour>) {
        let _ = swarm
            .behaviour_mut()
            .kad
            .start_providing(kad::RecordKey::new(&self.group));
        self.provide_due = false;
        self.last_provide = Some(Instant::now());
    }

    fn command(&mut self, swarm: &mut Swarm<Behaviour>, command: Command) {
        match command {
            Command::Rpc {
                id,
                group,
                node,
                protocol,
                payload,
                timeout,
            } => {
                let name = &self.rpc_protocols[protocol as usize];
                let mut call = Call {
                    group,
                    protocol,
                    frame: wire::encode_request(&self.delegation, name, &payload),
                    candidates: VecDeque::new(),
                    seen: HashSet::new(),
                    resolved: HashSet::new(),
                    in_flight: false,
                    resolving: false,
                    lookup_running: false,
                    deadline: Instant::now() + timeout,
                    last_failure: LP2P_FAIL_UNREACHABLE,
                };
                match node {
                    Some(key) => {
                        if let Some(peer) = keys::peer_id(&key) {
                            call.seen.insert(peer);
                            call.candidates.push_back(peer);
                        }
                    }
                    None => {
                        let query = swarm
                            .behaviour_mut()
                            .kad
                            .get_providers(kad::RecordKey::new(&group));
                        call.lookup_running = true;
                        self.provider_lookups.insert(query, id);
                    }
                }
                self.calls.insert(id, call);
                self.advance(swarm, id);
            }
            Command::Respond { id, payload } => {
                if let Some(channel) = self.inbound.remove(&id) {
                    let frame = wire::encode_response(&self.delegation, &payload);
                    // An error means the requester is gone; there is nobody left to tell.
                    let _ = swarm.behaviour_mut().rpc.send_response(channel, frame);
                }
            }
            Command::Reject { id } => {
                // Dropping the channel closes the stream without a response.
                self.inbound.remove(&id);
            }
            Command::AddAddress { node, address } => {
                if let Some(peer) = keys::peer_id(&node) {
                    swarm.behaviour_mut().address_book.add(peer, address.clone());
                    swarm.behaviour_mut().kad.add_address(&peer, address);
                }
            }
            Command::Bootstrap { id } => match swarm.behaviour_mut().kad.bootstrap() {
                Ok(query) => {
                    self.bootstraps.insert(query, id);
                }
                Err(_) => self.emit(
                    EventBuilder::new(LP2P_EV_DHT_RESULT)
                        .id(id)
                        .flags(LP2P_EVF_LAST)
                        .reason(LP2P_FAIL_UNREACHABLE)
                        .build(),
                ),
            },
            Command::RandomWalk { id } => {
                let query = swarm.behaviour_mut().kad.get_closest_peers(PeerId::random());
                self.walks.insert(query, id);
            }
            Command::RoutingSample { max, reply } => {
                let _ = reply.send(self.routing_sample(swarm, max));
            }
            Command::Shutdown => {}
        }
    }

    /// Sends the call to its next candidate, looks a candidate's addresses up first when nothing
    /// is known about it, or fails the call when nobody is left.
    fn advance(&mut self, swarm: &mut Swarm<Behaviour>, id: u64) {
        let Some(call) = self.calls.get_mut(&id) else {
            return;
        };
        if call.in_flight || call.resolving {
            return;
        }
        while let Some(peer) = call.candidates.pop_front() {
            let dialable = swarm.is_connected(&peer) || swarm.behaviour().address_book.knows(&peer);
            if !dialable {
                if call.resolved.insert(peer) {
                    let query = swarm.behaviour_mut().kad.get_closest_peers(peer);
                    self.resolutions.insert(query, id);
                    call.candidates.push_front(peer);
                    call.resolving = true;
                    return;
                }
                continue;
            }
            let request = swarm.behaviour_mut().rpc.send_request(&peer, call.frame.clone());
            self.outbound.insert(request, id);
            call.in_flight = true;
            self.shared.rpc_out.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if call.lookup_running {
            return;
        }
        if let Some(call) = self.calls.remove(&id) {
            self.emit(
                EventBuilder::new(LP2P_EV_RPC_FAILED)
                    .id(id)
                    .group(call.group)
                    .protocol(call.protocol)
                    .reason(call.last_failure)
                    .build(),
            );
        }
    }

    fn tick(&mut self, swarm: &mut Swarm<Behaviour>) {
        let now = Instant::now();
        let expired: Vec<u64> = self
            .calls
            .iter()
            .filter(|(_, call)| call.deadline <= now)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            if let Some(call) = self.calls.remove(&id) {
                // A response that still arrives finds no call and is dropped.
                self.emit(
                    EventBuilder::new(LP2P_EV_RPC_FAILED)
                        .id(id)
                        .group(call.group)
                        .protocol(call.protocol)
                        .reason(LP2P_FAIL_TIMEOUT)
                        .build(),
                );
            }
        }
        self.reserve(swarm);
        if self.provide_due
            && self
                .last_provide
                .is_none_or(|t| now.duration_since(t) >= PROVIDE_DEBOUNCE)
        {
            self.provide(swarm);
        }
        let routing: usize = swarm
            .behaviour_mut()
            .kad
            .kbuckets()
            .map(|bucket| bucket.num_entries())
            .sum();
        self.shared
            .routing_table_peers
            .store(routing as u32, Ordering::Relaxed);
        self.shared
            .connections
            .store(swarm.network_info().num_peers() as u32, Ordering::Relaxed);
    }

    /// Keeps a private node reachable: a reservation on up to MAX_RESERVATIONS relays among the
    /// peers that offered, skipping relays that failed recently.
    fn reserve(&mut self, swarm: &mut Swarm<Behaviour>) {
        if !self.private || !self.relay_client || self.reservations.len() >= MAX_RESERVATIONS {
            return;
        }
        let now = Instant::now();
        let candidates: Vec<(PeerId, Multiaddr)> = self
            .relay_candidates
            .iter()
            .filter(|(peer, _)| !self.reservations.contains_key(peer))
            .filter(|(peer, _)| {
                self.relay_failed
                    .get(peer)
                    .is_none_or(|t| now.duration_since(*t) >= RELAY_RETRY)
            })
            .map(|(peer, address)| (*peer, address.clone()))
            .collect();
        for (peer, address) in candidates {
            if self.reservations.len() >= MAX_RESERVATIONS {
                break;
            }
            let circuit = without_peer(&address)
                .with(Protocol::P2p(peer))
                .with(Protocol::P2pCircuit);
            match swarm.listen_on(circuit) {
                Ok(listener) => {
                    self.reservations.insert(peer, listener);
                }
                Err(_) => {
                    self.relay_failed.insert(peer, now);
                }
            }
        }
    }

    fn relay_lost(&mut self, listener: ListenerId) {
        let lost: Vec<PeerId> = self
            .reservations
            .iter()
            .filter(|(_, id)| **id == listener)
            .map(|(peer, _)| *peer)
            .collect();
        for peer in lost {
            self.reservations.remove(&peer);
            self.relay_failed.insert(peer, Instant::now());
        }
    }

    fn identified(&mut self, swarm: &mut Swarm<Behaviour>, peer: PeerId, info: identify::Info) {
        let speaks_dht = info.protocols.contains(&self.dht_protocol);
        if info.protocols.contains(&relay::HOP_PROTOCOL_NAME)
            && self.relay_candidates.len() < MAX_RELAY_CANDIDATES
        {
            // The address this node dialed is known to work; a listen address is a guess.
            let address = self.dialed.get(&peer).cloned().or_else(|| {
                info.listen_addrs
                    .iter()
                    .find(|a| !is_relayed(a) && !is_unspecified(a))
                    .cloned()
            });
            if let Some(address) = address {
                self.relay_candidates.insert(peer, address);
            }
        }
        for address in info.listen_addrs {
            swarm.behaviour_mut().address_book.add(peer, address.clone());
            if speaks_dht {
                swarm.behaviour_mut().kad.add_address(&peer, address);
            }
        }
        self.reserve(swarm);
    }

    fn routing_sample(&mut self, swarm: &mut Swarm<Behaviour>, max: usize) -> Vec<Record> {
        let mut entries: Vec<(PeerId, Vec<Multiaddr>)> = Vec::new();
        for bucket in swarm.behaviour_mut().kad.kbuckets() {
            for entry in bucket.iter() {
                entries.push((
                    *entry.node.key.preimage(),
                    entry.node.value.iter().cloned().collect(),
                ));
            }
        }
        if entries.is_empty() {
            return Vec::new();
        }
        // Buckets run from near to far. Starting somewhere else on every call hands out different
        // entry points instead of the same nearest few.
        self.sample_offset = (self.sample_offset + 1) % entries.len();
        entries
            .iter()
            .cycle()
            .skip(self.sample_offset)
            .take(max.min(entries.len()))
            .map(|(peer, addresses)| discovered(0, peer, addresses))
            .collect()
    }

    fn swarm_event(&mut self, swarm: &mut Swarm<Behaviour>, event: SwarmEvent<BehaviourEvent>) {
        match event {
            SwarmEvent::NewListenAddr { address, .. } => {
                if is_relayed(&address) {
                    // A reservation was accepted: this is how others reach this node now. Peers
                    // hear about it at once, and the provider record is re-published with it.
                    swarm.add_external_address(address.clone());
                    let peers: Vec<PeerId> = swarm.connected_peers().copied().collect();
                    swarm.behaviour_mut().identify.push(peers);
                    self.provide_due = true;
                } else if !self.private && !is_unspecified(&address) {
                    // A specific address a public node was told to listen on is taken as
                    // reachable. A private node announces only relayed addresses.
                    swarm.add_external_address(address.clone());
                }
                self.emit(EventBuilder::new(LP2P_EV_LISTENING).data(address.to_string().as_bytes()));
            }
            SwarmEvent::ListenerClosed { listener_id, .. } => self.relay_lost(listener_id),
            SwarmEvent::ListenerError { listener_id, .. } => self.relay_lost(listener_id),
            SwarmEvent::ConnectionEstablished {
                peer_id,
                num_established,
                endpoint,
                ..
            } => {
                if endpoint.is_dialer() && !is_relayed(endpoint.get_remote_address()) {
                    self.dialed.insert(peer_id, endpoint.get_remote_address().clone());
                }
                if num_established.get() == 1 {
                    let address = match &endpoint {
                        libp2p::core::ConnectedPoint::Dialer { address, .. } => address,
                        libp2p::core::ConnectedPoint::Listener { send_back_addr, .. } => send_back_addr,
                    };
                    self.emit(
                        EventBuilder::new(LP2P_EV_PEER_CONNECTED)
                            .node(peer_key(&peer_id))
                            .data(address.to_string().as_bytes()),
                    );
                }
            }
            SwarmEvent::ConnectionClosed {
                peer_id,
                num_established,
                ..
            } => {
                if num_established == 0 {
                    self.dialed.remove(&peer_id);
                    self.emit(
                        EventBuilder::new(LP2P_EV_PEER_DISCONNECTED)
                            .node(peer_key(&peer_id))
                            .build(),
                    );
                }
            }
            SwarmEvent::Behaviour(BehaviourEvent::Kad(event)) => self.kad_event(swarm, event),
            SwarmEvent::Behaviour(BehaviourEvent::Identify(
                identify::Event::Received { peer_id, info, .. }
                | identify::Event::Pushed { peer_id, info, .. },
            )) => self.identified(swarm, peer_id, info),
            SwarmEvent::Behaviour(BehaviourEvent::Rpc(event)) => self.rpc_event(swarm, event),
            _ => {}
        }
    }

    fn kad_event(&mut self, swarm: &mut Swarm<Behaviour>, event: kad::Event) {
        match event {
            kad::Event::RoutingUpdated {
                peer,
                addresses,
                is_new_peer,
                ..
            } => {
                for address in addresses.iter() {
                    swarm.behaviour_mut().address_book.add(peer, address.clone());
                }
                if is_new_peer {
                    self.provide_due = true;
                }
            }
            kad::Event::OutboundQueryProgressed {
                id: query,
                result,
                step,
                ..
            } => match result {
                kad::QueryResult::GetProviders(result) => {
                    let Some(&call_id) = self.provider_lookups.get(&query) else {
                        return;
                    };
                    if let Some(call) = self.calls.get_mut(&call_id) {
                        if let Ok(kad::GetProvidersOk::FoundProviders { providers, .. }) = result {
                            for provider in providers {
                                if provider != self.local_peer {
                                    enqueue(call, provider, &self.last_success, &self.failed);
                                }
                            }
                        }
                        if step.last {
                            call.lookup_running = false;
                        }
                    }
                    if step.last {
                        self.provider_lookups.remove(&query);
                    }
                    self.advance(swarm, call_id);
                }
                kad::QueryResult::GetClosestPeers(result) => {
                    let peers = match result {
                        Ok(ok) => ok.peers,
                        Err(kad::GetClosestPeersError::Timeout { peers, .. }) => peers,
                    };
                    for info in &peers {
                        for address in &info.addrs {
                            swarm
                                .behaviour_mut()
                                .address_book
                                .add(info.peer_id, address.clone());
                        }
                    }
                    if let Some(call_id) = self.resolutions.remove(&query) {
                        if let Some(call) = self.calls.get_mut(&call_id) {
                            call.resolving = false;
                        }
                        self.advance(swarm, call_id);
                    } else if let Some(walk) = self.walks.remove(&query) {
                        for info in &peers {
                            self.emit(discovered(walk, &info.peer_id, &info.addrs));
                        }
                        self.emit(
                            EventBuilder::new(LP2P_EV_DHT_RESULT)
                                .id(walk)
                                .flags(LP2P_EVF_LAST)
                                .build(),
                        );
                    }
                }
                kad::QueryResult::Bootstrap(result) => {
                    if step.last {
                        if let Some(id) = self.bootstraps.remove(&query) {
                            let reason = if result.is_ok() { 0 } else { LP2P_FAIL_TIMEOUT };
                            self.emit(
                                EventBuilder::new(LP2P_EV_DHT_RESULT)
                                    .id(id)
                                    .flags(LP2P_EVF_LAST)
                                    .reason(reason)
                                    .build(),
                            );
                        }
                        self.provide_due = true;
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    fn rpc_event(&mut self, swarm: &mut Swarm<Behaviour>, event: request_response::Event<Vec<u8>, Vec<u8>>) {
        match event {
            request_response::Event::Message { peer, message, .. } => match message {
                request_response::Message::Request {
                    request_id,
                    request,
                    channel,
                } => self.inbound_request(peer, request_id, &request, channel),
                request_response::Message::Response { request_id, response } => {
                    self.response(swarm, peer, request_id, &response)
                }
            },
            request_response::Event::OutboundFailure {
                peer,
                request_id,
                error,
                ..
            } => {
                self.remember_failure(peer);
                let Some(id) = self.outbound.remove(&request_id) else {
                    return;
                };
                if let Some(call) = self.calls.get_mut(&id) {
                    call.in_flight = false;
                    call.last_failure = match error {
                        request_response::OutboundFailure::Timeout => LP2P_FAIL_TIMEOUT,
                        request_response::OutboundFailure::UnsupportedProtocols => LP2P_FAIL_REFUSED,
                        _ => LP2P_FAIL_UNREACHABLE,
                    };
                }
                self.advance(swarm, id);
            }
            request_response::Event::InboundFailure { request_id, .. }
            | request_response::Event::ResponseSent { request_id, .. } => {
                if let Some(id) = self.inbound_ids.remove(&request_id) {
                    self.inbound.remove(&id);
                }
            }
        }
    }

    fn inbound_request(
        &mut self,
        peer: PeerId,
        request_id: InboundRequestId,
        frame: &[u8],
        channel: ResponseChannel<Vec<u8>>,
    ) {
        // Every early return drops the channel, which is a rejection: the requester's failover
        // moves on without waiting for a timeout.
        let Some(request) = wire::decode_request(frame) else {
            return;
        };
        let Some(node) = keys::key_of(&peer) else {
            return;
        };
        let Ok(delegation) = Delegation::parse(request.delegation) else {
            return;
        };
        if delegation.verify_for(&node, now_ms()).is_err() {
            return;
        }
        let Some(protocol) = self.rpc_protocols.iter().position(|p| p == request.protocol) else {
            return;
        };
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        self.shared.rpc_in.fetch_add(1, Ordering::Relaxed);
        self.emit(
            EventBuilder::new(LP2P_EV_RPC_REQUEST)
                .id(id)
                .group(delegation.group)
                .node(node)
                .protocol(protocol as u16)
                .data(request.payload),
        );
        self.inbound.insert(id, channel);
        self.inbound_ids.insert(request_id, id);
    }

    fn response(
        &mut self,
        swarm: &mut Swarm<Behaviour>,
        peer: PeerId,
        request_id: OutboundRequestId,
        frame: &[u8],
    ) {
        let Some(id) = self.outbound.remove(&request_id) else {
            return;
        };
        let Some(call) = self.calls.get_mut(&id) else {
            return;
        };
        call.in_flight = false;
        let group = call.group;
        let checked = (|| {
            let response = wire::decode_response(frame)?;
            let node = keys::key_of(&peer)?;
            let delegation = Delegation::parse(response.delegation).ok()?;
            delegation.verify_for(&node, now_ms()).ok()?;
            (delegation.group == group).then_some((node, response.payload))
        })();
        match checked {
            Some((node, payload)) => {
                let protocol = call.protocol;
                self.calls.remove(&id);
                self.failed.remove(&peer);
                if self.last_success.len() >= MAX_REMEMBERED {
                    self.last_success.clear();
                }
                self.last_success.insert(group, peer);
                self.emit(
                    EventBuilder::new(LP2P_EV_RPC_RESPONSE)
                        .id(id)
                        .group(group)
                        .node(node)
                        .protocol(protocol)
                        .data(payload),
                );
            }
            None => {
                // An empty or malformed frame is a rejection, a valid one from another group is an
                // impostor. Either way this node is done and the next one is asked.
                call.last_failure = LP2P_FAIL_REFUSED;
                self.remember_failure(peer);
                self.advance(swarm, id);
            }
        }
    }

    fn remember_failure(&mut self, peer: PeerId) {
        if self.failed.len() >= MAX_REMEMBERED {
            let now = Instant::now();
            self.failed
                .retain(|_, t| now.duration_since(*t) < FAILURE_PENALTY);
            if self.failed.len() >= MAX_REMEMBERED {
                self.failed.clear();
            }
        }
        self.failed.insert(peer, Instant::now());
    }
}

fn discovered(id: u64, peer: &PeerId, addresses: &[Multiaddr]) -> Record {
    let mut data = Vec::new();
    for address in addresses {
        data.extend_from_slice(address.to_string().as_bytes());
        data.push(0);
    }
    EventBuilder::new(LP2P_EV_PEER_DISCOVERED)
        .id(id)
        .node(peer_key(peer))
        .data(&data)
}
