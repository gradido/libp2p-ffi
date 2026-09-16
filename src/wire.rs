//! What travels on an RPC stream. Everything here is the contract a mirror in another language
//! has to follow byte for byte.
//!
//! One libp2p protocol carries every RPC: `/lp2p/rpc/1`. A stream holds one request and one
//! response. The requester writes its frame and closes its write side; the responder reads to the
//! end, writes its frame and closes; the requester reads to the end. A stream closed without a
//! response is a rejection.
//!
//! ```text
//! request    u8 version = 1
//!            136 bytes  the requester's delegation
//!            u8         length of the protocol name, 1..=255
//!            ...        the protocol name, UTF-8, as the caller registered it
//!            ...        payload, to the end of the stream
//!
//! response   u8 version = 1
//!            136 bytes  the responder's delegation
//!            ...        payload, to the end of the stream
//! ```
//!
//! The protocol name rides in the frame rather than in libp2p's protocol negotiation because
//! rust-libp2p's request-response cannot choose a protocol per request. The delegation rides in
//! every frame so that neither side needs a second round trip, or any state, to know which group
//! the other belongs to.

use std::io;

use async_trait::async_trait;
use futures::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use libp2p::StreamProtocol;
use libp2p::request_response;

use crate::abi::LP2P_DELEGATION_BYTES;

pub const RPC_PROTOCOL: StreamProtocol = StreamProtocol::new("/lp2p/rpc/1");

/// The Kademlia key every node of a group provides under: the sha2-256 multihash of the group key,
/// `0x12 0x20 || sha256(group key)`.
///
/// Not the raw 32 bytes: js-libp2p's DHT provides and looks up only by CID, whose key is a
/// multihash. A CIDv1 with the raw codec over this digest names the same key there.
pub fn provider_key(group: &[u8; 32]) -> Vec<u8> {
    use sha2::Digest;
    let mut key = Vec::with_capacity(34);
    key.extend_from_slice(&[0x12, 0x20]);
    key.extend_from_slice(&sha2::Sha256::digest(group));
    key
}
const VERSION: u8 = 1;

/// The gossipsub topic a 32-byte topic key names: `/lp2p/topic/1/<key in lowercase hex>`, hashed
/// by identity, so the string itself is what travels. A mirror in another language builds the same
/// string from the same key.
pub fn topic_name(key: &[u8; 32]) -> String {
    let mut name = String::with_capacity(14 + 64);
    name.push_str("/lp2p/topic/1/");
    for byte in key {
        name.push_str(&format!("{byte:02x}"));
    }
    name
}

pub struct Request<'a> {
    pub delegation: &'a [u8],
    pub protocol: &'a str,
    pub payload: &'a [u8],
}

pub struct Response<'a> {
    pub delegation: &'a [u8],
    pub payload: &'a [u8],
}

pub fn encode_request(delegation: &[u8], protocol: &str, payload: &[u8]) -> Vec<u8> {
    debug_assert_eq!(delegation.len(), LP2P_DELEGATION_BYTES);
    debug_assert!(!protocol.is_empty() && protocol.len() <= 255);
    let mut out = Vec::with_capacity(2 + LP2P_DELEGATION_BYTES + protocol.len() + payload.len());
    out.push(VERSION);
    out.extend_from_slice(delegation);
    out.push(protocol.len() as u8);
    out.extend_from_slice(protocol.as_bytes());
    out.extend_from_slice(payload);
    out
}

pub fn decode_request(frame: &[u8]) -> Option<Request<'_>> {
    let (&version, rest) = frame.split_first()?;
    if version != VERSION || rest.len() < LP2P_DELEGATION_BYTES + 1 {
        return None;
    }
    let (delegation, rest) = rest.split_at(LP2P_DELEGATION_BYTES);
    let (&name_len, rest) = rest.split_first()?;
    let name_len = name_len as usize;
    if name_len == 0 || rest.len() < name_len {
        return None;
    }
    let (name, payload) = rest.split_at(name_len);
    Some(Request {
        delegation,
        protocol: std::str::from_utf8(name).ok()?,
        payload,
    })
}

/// An announcement: the same shape as a response, carried in a gossipsub message the node signs.
///
/// ```text
/// announcement   u8 version = 1 | 136 bytes the announcing node's delegation | payload
/// ```
pub fn encode_announcement(delegation: &[u8], payload: &[u8]) -> Vec<u8> {
    encode_response(delegation, payload)
}

pub fn decode_announcement(frame: &[u8]) -> Option<Response<'_>> {
    decode_response(frame)
}

pub fn encode_response(delegation: &[u8], payload: &[u8]) -> Vec<u8> {
    debug_assert_eq!(delegation.len(), LP2P_DELEGATION_BYTES);
    let mut out = Vec::with_capacity(1 + LP2P_DELEGATION_BYTES + payload.len());
    out.push(VERSION);
    out.extend_from_slice(delegation);
    out.extend_from_slice(payload);
    out
}

pub fn decode_response(frame: &[u8]) -> Option<Response<'_>> {
    let (&version, rest) = frame.split_first()?;
    if version != VERSION || rest.len() < LP2P_DELEGATION_BYTES {
        return None;
    }
    let (delegation, payload) = rest.split_at(LP2P_DELEGATION_BYTES);
    Some(Response { delegation, payload })
}

/// Bytes in, bytes out, bounded. The frame overhead is added to the caller's payload limits.
#[derive(Clone)]
pub struct RawCodec {
    pub max_request: u64,
    pub max_response: u64,
}

async fn read_bounded<T: AsyncRead + Unpin + Send>(io: &mut T, max: u64) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    // One byte past the limit tells a frame that is exactly at the limit from one that is over.
    io.take(max + 1).read_to_end(&mut buf).await?;
    if buf.len() as u64 > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame exceeds the limit",
        ));
    }
    Ok(buf)
}

#[async_trait]
impl request_response::Codec for RawCodec {
    type Protocol = StreamProtocol;
    type Request = Vec<u8>;
    type Response = Vec<u8>;

    async fn read_request<T>(&mut self, _: &Self::Protocol, io: &mut T) -> io::Result<Vec<u8>>
    where
        T: AsyncRead + Unpin + Send,
    {
        read_bounded(io, self.max_request).await
    }

    async fn read_response<T>(&mut self, _: &Self::Protocol, io: &mut T) -> io::Result<Vec<u8>>
    where
        T: AsyncRead + Unpin + Send,
    {
        read_bounded(io, self.max_response).await
    }

    async fn write_request<T>(&mut self, _: &Self::Protocol, io: &mut T, req: Vec<u8>) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        io.write_all(&req).await
    }

    async fn write_response<T>(&mut self, _: &Self::Protocol, io: &mut T, res: Vec<u8>) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        io.write_all(&res).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let delegation = [9u8; LP2P_DELEGATION_BYTES];
        let frame = encode_request(&delegation, "/gradido/federation/1", b"hello");
        let req = decode_request(&frame).unwrap();
        assert_eq!(req.delegation, &delegation[..]);
        assert_eq!(req.protocol, "/gradido/federation/1");
        assert_eq!(req.payload, b"hello");

        let frame = encode_response(&delegation, b"");
        let res = decode_response(&frame).unwrap();
        assert_eq!(res.payload, b"");
    }

    #[test]
    fn the_provider_key_is_the_multihash_js_libp2p_names_by_cid() {
        // CID.createV1(0x55, sha256.digest(32 bytes of 1)).multihash.bytes, from interop/js.
        let expected = "122072cd6e8422c407fb6d098690f1130b7ded7ec2f7f5e1d30bd9d521f015363793";
        let key: String = provider_key(&[1; 32])
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(key, expected);
    }

    #[test]
    fn a_topic_key_names_one_string() {
        assert_eq!(
            topic_name(&[0xab; 32]),
            "/lp2p/topic/1/abababababababababababababababababababababababababababababababab"
        );
    }

    #[test]
    fn truncated_frames_are_refused() {
        let delegation = [9u8; LP2P_DELEGATION_BYTES];
        let frame = encode_request(&delegation, "/p", b"x");
        assert!(decode_request(&frame[..10]).is_none());
        assert!(decode_request(&[2]).is_none());
        assert!(decode_response(&frame[..100]).is_none());
    }
}
