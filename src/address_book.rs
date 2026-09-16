//! Addresses of peers that are not in the routing table.
//!
//! Kademlia hands addresses to a dial only for peers in its buckets and for peers of a query that
//! is still running. A provider found by a finished lookup, or a node the caller named, would
//! otherwise be undialable. This behaviour remembers what the node learned and offers it to every
//! outbound dial. It is bounded: when full, the oldest peer is forgotten.

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::task::{Context, Poll};

use libp2p::core::Endpoint;
use libp2p::core::transport::PortUse;
use libp2p::swarm::{
    ConnectionDenied, ConnectionId, FromSwarm, NetworkBehaviour, THandler, THandlerInEvent, THandlerOutEvent,
    ToSwarm, dummy,
};
use libp2p::{Multiaddr, PeerId};

const MAX_PEERS: usize = 4096;
const MAX_ADDRESSES_PER_PEER: usize = 8;

#[derive(Default)]
pub struct AddressBook {
    addresses: HashMap<PeerId, Vec<Multiaddr>>,
    order: VecDeque<PeerId>,
}

impl AddressBook {
    pub fn add(&mut self, peer: PeerId, address: Multiaddr) {
        if let Some(list) = self.addresses.get_mut(&peer) {
            if !list.contains(&address) {
                if list.len() == MAX_ADDRESSES_PER_PEER {
                    list.remove(0);
                }
                list.push(address);
            }
            return;
        }
        if self.addresses.len() == MAX_PEERS
            && let Some(oldest) = self.order.pop_front()
        {
            self.addresses.remove(&oldest);
        }
        self.addresses.insert(peer, vec![address]);
        self.order.push_back(peer);
    }

    pub fn addresses(&self, peer: &PeerId) -> &[Multiaddr] {
        self.addresses.get(peer).map_or(&[], Vec::as_slice)
    }

    pub fn knows(&self, peer: &PeerId) -> bool {
        self.addresses.contains_key(peer)
    }
}

impl NetworkBehaviour for AddressBook {
    type ConnectionHandler = dummy::ConnectionHandler;
    type ToSwarm = Infallible;

    fn handle_established_inbound_connection(
        &mut self,
        _: ConnectionId,
        _: PeerId,
        _: &Multiaddr,
        _: &Multiaddr,
    ) -> Result<THandler<Self>, ConnectionDenied> {
        Ok(dummy::ConnectionHandler)
    }

    fn handle_pending_outbound_connection(
        &mut self,
        _: ConnectionId,
        maybe_peer: Option<PeerId>,
        _: &[Multiaddr],
        _: Endpoint,
    ) -> Result<Vec<Multiaddr>, ConnectionDenied> {
        Ok(maybe_peer
            .and_then(|peer| self.addresses.get(&peer).cloned())
            .unwrap_or_default())
    }

    fn handle_established_outbound_connection(
        &mut self,
        _: ConnectionId,
        _: PeerId,
        _: &Multiaddr,
        _: Endpoint,
        _: PortUse,
    ) -> Result<THandler<Self>, ConnectionDenied> {
        Ok(dummy::ConnectionHandler)
    }

    fn on_swarm_event(&mut self, event: FromSwarm) {
        if let FromSwarm::NewExternalAddrOfPeer(e) = event {
            self.add(e.peer_id, e.addr.clone());
        }
    }

    fn on_connection_handler_event(&mut self, _: PeerId, _: ConnectionId, event: THandlerOutEvent<Self>) {
        match event {}
    }

    fn poll(&mut self, _: &mut Context<'_>) -> Poll<ToSwarm<Self::ToSwarm, THandlerInEvent<Self>>> {
        Poll::Pending
    }
}
