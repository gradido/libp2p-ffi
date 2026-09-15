//! A node's key is its peer id. These convert between the 32 bytes the C side sees and libp2p's
//! peer id, and refuse every key type but ed25519.

use libp2p::PeerId;
use libp2p::identity::{Keypair, PublicKey, ed25519};

use crate::abi::lp2p_key;

pub fn keypair_from_seed(seed: &[u8; 32]) -> Option<Keypair> {
    let mut copy = *seed;
    Keypair::ed25519_from_bytes(&mut copy).ok()
}

pub fn public_key(keypair: &Keypair) -> Option<lp2p_key> {
    keypair.public().try_into_ed25519().ok().map(|pk| pk.to_bytes())
}

pub fn peer_id(key: &lp2p_key) -> Option<PeerId> {
    let pk = ed25519::PublicKey::try_from_bytes(key).ok()?;
    Some(PublicKey::from(pk).to_peer_id())
}

/// The ed25519 key inside a peer id, or None for any other kind of peer.
pub fn key_of(peer: &PeerId) -> Option<lp2p_key> {
    let multihash = peer.as_ref();
    // An ed25519 peer id is the identity multihash of the protobuf-encoded public key.
    if multihash.code() != 0 {
        return None;
    }
    let pk = PublicKey::try_decode_protobuf(multihash.digest()).ok()?;
    pk.try_into_ed25519().ok().map(|pk| pk.to_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_id_round_trips_through_the_key() {
        let keypair = keypair_from_seed(&[7; 32]).unwrap();
        let key = public_key(&keypair).unwrap();
        let peer = peer_id(&key).unwrap();
        assert_eq!(peer, keypair.public().to_peer_id());
        assert_eq!(key_of(&peer), Some(key));
    }
}
