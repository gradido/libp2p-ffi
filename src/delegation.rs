//! "Node key X belongs to group Y until T", signed by the group key.
//!
//! The module has to check a delegation on every request and every response, so the format is
//! the module's and not the caller's:
//!
//! ```text
//! offset  size  field
//!      0    32  node key     ed25519 public key; the peer id of the node
//!     32    32  group key    ed25519 public key
//!     64     8  expires_ms   unix milliseconds, big endian; 0 means it does not expire
//!     72    64  signature    ed25519 by the group key over
//!                            "libp2p-ffi delegation v1" || node key || group key || expires_ms
//! ```
//!
//! The context string keeps a delegation from ever being a valid signature over anything else the
//! group key signs.

use libp2p::identity::ed25519;

use crate::abi::{LP2P_DELEGATION_BYTES, lp2p_key};

const CONTEXT: &[u8] = b"libp2p-ffi delegation v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Delegation {
    pub node: lp2p_key,
    pub group: lp2p_key,
    pub expires_ms: u64,
    pub signature: [u8; 64],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Invalid {
    Length,
    GroupKey,
    Signature,
    Expired,
    WrongNode,
}

fn message(node: &lp2p_key, group: &lp2p_key, expires_ms: u64) -> Vec<u8> {
    let mut msg = Vec::with_capacity(CONTEXT.len() + 72);
    msg.extend_from_slice(CONTEXT);
    msg.extend_from_slice(node);
    msg.extend_from_slice(group);
    msg.extend_from_slice(&expires_ms.to_be_bytes());
    msg
}

impl Delegation {
    pub fn parse(bytes: &[u8]) -> Result<Self, Invalid> {
        if bytes.len() != LP2P_DELEGATION_BYTES {
            return Err(Invalid::Length);
        }
        let mut d = Delegation {
            node: [0; 32],
            group: [0; 32],
            expires_ms: 0,
            signature: [0; 64],
        };
        d.node.copy_from_slice(&bytes[0..32]);
        d.group.copy_from_slice(&bytes[32..64]);
        let mut expires = [0u8; 8];
        expires.copy_from_slice(&bytes[64..72]);
        d.expires_ms = u64::from_be_bytes(expires);
        d.signature.copy_from_slice(&bytes[72..136]);
        Ok(d)
    }

    pub fn to_bytes(&self) -> [u8; LP2P_DELEGATION_BYTES] {
        let mut out = [0u8; LP2P_DELEGATION_BYTES];
        out[0..32].copy_from_slice(&self.node);
        out[32..64].copy_from_slice(&self.group);
        out[64..72].copy_from_slice(&self.expires_ms.to_be_bytes());
        out[72..136].copy_from_slice(&self.signature);
        out
    }

    pub fn sign(group_seed: &[u8; 32], node: &lp2p_key, expires_ms: u64) -> Option<Self> {
        let mut seed = *group_seed;
        let secret = ed25519::SecretKey::try_from_bytes(&mut seed).ok()?;
        let keypair = ed25519::Keypair::from(secret);
        let group = keypair.public().to_bytes();
        let signature: [u8; 64] = keypair.sign(&message(node, &group, expires_ms)).try_into().ok()?;
        Some(Delegation {
            node: *node,
            group,
            expires_ms,
            signature,
        })
    }

    /// Checks the signature and the expiry against @p now_ms.
    pub fn verify(&self, now_ms: u64) -> Result<(), Invalid> {
        let group = ed25519::PublicKey::try_from_bytes(&self.group).map_err(|_| Invalid::GroupKey)?;
        if !group.verify(
            &message(&self.node, &self.group, self.expires_ms),
            &self.signature,
        ) {
            return Err(Invalid::Signature);
        }
        if self.expires_ms != 0 && self.expires_ms <= now_ms {
            return Err(Invalid::Expired);
        }
        Ok(())
    }

    /// Checks everything [`verify`](Self::verify) does, and that the delegation names @p node.
    pub fn verify_for(&self, node: &lp2p_key, now_ms: u64) -> Result<(), Invalid> {
        self.verify(now_ms)?;
        if &self.node != node {
            return Err(Invalid::WrongNode);
        }
        Ok(())
    }
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_signed_delegation_verifies_and_survives_the_round_trip() {
        let d = Delegation::sign(&[1; 32], &[2; 32], 0).unwrap();
        let parsed = Delegation::parse(&d.to_bytes()).unwrap();
        assert_eq!(parsed, d);
        assert_eq!(parsed.verify_for(&[2; 32], now_ms()), Ok(()));
    }

    #[test]
    fn anything_changed_is_refused() {
        let d = Delegation::sign(&[1; 32], &[2; 32], 0).unwrap();
        let mut other_node = d;
        other_node.node = [3; 32];
        assert_eq!(other_node.verify(now_ms()), Err(Invalid::Signature));
        assert_eq!(d.verify_for(&[3; 32], now_ms()), Err(Invalid::WrongNode));

        let expired = Delegation::sign(&[1; 32], &[2; 32], 1000).unwrap();
        assert_eq!(expired.verify(now_ms()), Err(Invalid::Expired));
        assert_eq!(Delegation::parse(&[0; 10]), Err(Invalid::Length));
    }
}
