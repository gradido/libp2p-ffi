/**
 * What a js-libp2p node has to do to be the same node as libp2p-ffi, written out once for the
 * interop tests. Everything here mirrors a file of the Rust module and has to stay byte for byte
 * equal to it:
 *
 *   delegation   src/delegation.rs
 *   frames       src/wire.rs
 *   provider key src/wire.rs, provider_key
 *   topic name   src/wire.rs, topic_name
 */
import { generateKeyPairFromSeed, publicKeyFromRaw } from '@libp2p/crypto/keys'
import type { Ed25519PrivateKey } from '@libp2p/interface'
import { CID } from 'multiformats/cid'
import { sha256 } from 'multiformats/hashes/sha2'

export const RPC_PROTOCOL = '/lp2p/rpc/1'
export const DELEGATION_BYTES = 136
const DELEGATION_CONTEXT = new TextEncoder().encode('libp2p-ffi delegation v1')
const VERSION = 1

export function hex(bytes: Uint8Array): string {
  return Buffer.from(bytes).toString('hex')
}

export function unhex(text: string): Uint8Array {
  return new Uint8Array(Buffer.from(text, 'hex'))
}

export function keyFromSeed(byte: number): Promise<Ed25519PrivateKey> {
  return generateKeyPairFromSeed('Ed25519', new Uint8Array(32).fill(byte))
}

function concat(...parts: Uint8Array[]): Uint8Array {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0))
  let offset = 0
  for (const part of parts) {
    out.set(part, offset)
    offset += part.length
  }
  return out
}

function signedPart(node: Uint8Array, group: Uint8Array, expiresMs: bigint): Uint8Array {
  const expires = new Uint8Array(8)
  new DataView(expires.buffer).setBigUint64(0, expiresMs, false)
  return concat(DELEGATION_CONTEXT, node, group, expires)
}

/** node key (32) | group key (32) | expires_ms big endian (8) | signature by the group key (64) */
export async function signDelegation(groupKey: Ed25519PrivateKey, node: Uint8Array, expiresMs = 0n): Promise<Uint8Array> {
  const group = groupKey.publicKey.raw
  const expires = new Uint8Array(8)
  new DataView(expires.buffer).setBigUint64(0, expiresMs, false)
  const signature = await groupKey.sign(signedPart(node, group, expiresMs))
  return concat(node, group, expires, signature)
}

export interface Delegation {
  node: Uint8Array
  group: Uint8Array
}

/** The delegation's keys if its signature holds and it has not expired, otherwise undefined. */
export async function verifyDelegation(bytes: Uint8Array): Promise<Delegation | undefined> {
  if (bytes.length !== DELEGATION_BYTES) return undefined
  const node = bytes.slice(0, 32)
  const group = bytes.slice(32, 64)
  const expiresMs = new DataView(bytes.buffer, bytes.byteOffset + 64, 8).getBigUint64(0, false)
  const signature = bytes.slice(72, 136)
  const valid = await publicKeyFromRaw(group).verify(signedPart(node, group, expiresMs), signature)
  if (!valid || (expiresMs !== 0n && expiresMs <= BigInt(Date.now()))) return undefined
  return { node, group }
}

export function requestFrame(delegation: Uint8Array, protocol: string, payload: Uint8Array): Uint8Array {
  const name = new TextEncoder().encode(protocol)
  return concat(new Uint8Array([VERSION]), delegation, new Uint8Array([name.length]), name, payload)
}

export function parseRequest(frame: Uint8Array): { delegation: Uint8Array; protocol: string; payload: Uint8Array } | undefined {
  if (frame.length < 2 + DELEGATION_BYTES || frame[0] !== VERSION) return undefined
  const delegation = frame.slice(1, 1 + DELEGATION_BYTES)
  const nameLength = frame[1 + DELEGATION_BYTES]
  const start = 2 + DELEGATION_BYTES
  if (nameLength === 0 || frame.length < start + nameLength) return undefined
  return {
    delegation,
    protocol: new TextDecoder().decode(frame.slice(start, start + nameLength)),
    payload: frame.slice(start + nameLength),
  }
}

/** Responses and announcements share one shape: version | delegation | payload. */
export function responseFrame(delegation: Uint8Array, payload: Uint8Array): Uint8Array {
  return concat(new Uint8Array([VERSION]), delegation, payload)
}

export function parseResponse(frame: Uint8Array): { delegation: Uint8Array; payload: Uint8Array } | undefined {
  if (frame.length < 1 + DELEGATION_BYTES || frame[0] !== VERSION) return undefined
  return { delegation: frame.slice(1, 1 + DELEGATION_BYTES), payload: frame.slice(1 + DELEGATION_BYTES) }
}

/** The gossipsub topic a 32-byte topic key names, as src/wire.rs builds it. */
export function topicName(key: Uint8Array): string {
  return `/lp2p/topic/1/${hex(key)}`
}

/** The Kademlia key a group's nodes provide under: the CID a js-libp2p DHT can name it by. */
export async function providerKey(group: Uint8Array): Promise<CID> {
  return CID.createV1(0x55, await sha256.digest(group))
}
