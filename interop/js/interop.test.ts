/**
 * js-libp2p under Bun against libp2p-ffi, on loopback. The Rust side is examples/interop_peer,
 * driven over stdin; run.sh builds it and passes its path as INTEROP_PEER.
 */
import { afterAll, beforeAll, expect, test } from 'bun:test'
import { noise } from '@chainsafe/libp2p-noise'
import { quic } from '@chainsafe/libp2p-quic'
import { circuitRelayServer, circuitRelayTransport } from '@libp2p/circuit-relay-v2'
import { yamux } from '@chainsafe/libp2p-yamux'
import { gossipsub } from '@libp2p/gossipsub'
import { identify } from '@libp2p/identify'
import type { Ed25519PrivateKey, PeerId, Stream } from '@libp2p/interface'
import { kadDHT, passthroughMapper } from '@libp2p/kad-dht'
import { peerIdFromString } from '@libp2p/peer-id'
import { ping } from '@libp2p/ping'
import { tcp } from '@libp2p/tcp'
import { multiaddr } from '@multiformats/multiaddr'
import { createLibp2p, type Libp2p } from 'libp2p'
import {
  RPC_PROTOCOL,
  hex,
  keyFromSeed,
  parseRequest,
  parseResponse,
  providerKey,
  requestFrame,
  responseFrame,
  signDelegation,
  topicName,
  unhex,
  verifyDelegation,
} from './lp2p.ts'

const DHT_PROTOCOL = '/interop/kad/1'
const ANNOUNCE_TOPIC = `${DHT_PROTOCOL}/announce`
const ECHO = '/interop/echo/1'
const encode = (text: string) => new TextEncoder().encode(text)
const decode = (bytes: Uint8Array) => new TextDecoder().decode(bytes)

/** The Rust node, as lines. */
class RustPeer {
  private lines: string[] = []
  private waiters: Array<() => void> = []
  readonly process: ReturnType<typeof Bun.spawn>
  peerId!: PeerId
  key!: string
  group!: string
  address!: string

  constructor(env: Record<string, string>) {
    const binary = process.env.INTEROP_PEER
    if (!binary) throw new Error('INTEROP_PEER is not set; run interop/js/run.sh')
    this.process = Bun.spawn([binary], { env: { ...process.env, ...env }, stdin: 'pipe', stdout: 'pipe', stderr: 'inherit' })
    this.read()
  }

  private async read() {
    const decoder = new TextDecoder()
    let pending = ''
    for await (const chunk of this.process.stdout as ReadableStream<Uint8Array>) {
      pending += decoder.decode(chunk)
      let newline: number
      while ((newline = pending.indexOf('\n')) >= 0) {
        const line = pending.slice(0, newline)
        pending = pending.slice(newline + 1)
        if (process.env.VERBOSE) console.log(`rust> ${line}`)
        this.lines.push(line)
        for (const wake of this.waiters.splice(0)) wake()
      }
    }
  }

  /** The first line, seen or still to come, whose first word is @p word (any, if empty) and that
   * passes @p match. Lines that do not match stay for later waits. */
  async waitFor(word: string, match: (words: string[]) => boolean = () => true, timeoutMs = 15_000): Promise<string[]> {
    const deadline = Date.now() + timeoutMs
    for (;;) {
      const index = this.lines.findIndex((l) => {
        const words = l.split(' ')
        return (word === '' || words[0] === word) && match(words)
      })
      if (index >= 0) return this.lines.splice(index, 1)[0].split(' ')
      if (Date.now() > deadline) throw new Error(`no "${word}" line within ${timeoutMs} ms; have: ${this.lines.join(' | ')}`)
      await new Promise<void>((resolve) => {
        this.waiters.push(resolve)
        setTimeout(resolve, 200)
      })
    }
  }

  send(command: string) {
    const stdin = this.process.stdin as import('bun').FileSink
    stdin.write(`${command}\n`)
    stdin.flush()
  }

  async start() {
    const [, peer, key, group] = await this.waitFor('READY')
    this.peerId = peerIdFromString(peer)
    this.key = key
    this.group = group
    const [, address] = await this.waitFor('LISTENING')
    this.address = `${address}/p2p/${peer}`
  }
}

/** The js node, configured the way a mirror of libp2p-ffi has to be. */
interface JsNode {
  node: Libp2p<any>
  key: Ed25519PrivateKey
  group: Ed25519PrivateKey
  delegation: Uint8Array
}

interface JsSetup {
  listen?: string[]
  relayServer?: boolean
}

async function startJs(seed: number, groupSeed: number, setup: JsSetup = {}): Promise<JsNode> {
  const key = await keyFromSeed(seed)
  const group = await keyFromSeed(groupSeed)
  const delegation = await signDelegation(group, key.publicKey.raw)
  const services: Record<string, any> = {
    identify: identify(),
    // kad-dht requires it: it pings routing-table peers before evicting them.
    ping: ping(),
    dht: kadDHT({ protocol: DHT_PROTOCOL, clientMode: false, peerInfoMapper: passthroughMapper }),
    pubsub: gossipsub({ allowPublishToZeroTopicPeers: true }),
  }
  if (setup.relayServer) services.relay = circuitRelayServer()
  const node = await createLibp2p({
    privateKey: key,
    addresses: { listen: setup.listen ?? ['/ip4/127.0.0.1/tcp/0', '/ip4/127.0.0.1/udp/0/quic-v1'] },
    // Every node can dial through a relay, as every libp2p-ffi node can.
    transports: [tcp(), quic(), circuitRelayTransport()],
    connectionEncrypters: [noise()],
    streamMuxers: [yamux()],
    services,
  })
  await node.handle(RPC_PROTOCOL, async (stream: Stream, connection) => {
    // Answer the way libp2p-ffi does: check the delegation, then respond "js:<payload>".
    const request = parseRequest(await readAll(stream))
    const checked = request && (await verifyDelegation(request.delegation))
    if (!request || !checked || hex(checked.node) !== hex(connection.remotePeer.publicKey!.raw)) {
      stream.abort(new Error('refused'))
      return
    }
    stream.send(responseFrame(delegation, encode(`js:${decode(request.payload)}`)))
    await stream.close()
    // A community without a URL is reached over a relay; js-libp2p refuses streams on relayed
    // ("limited") connections unless the protocol says otherwise.
  }, { runOnLimitedConnection: true })
  return { node, key, group, delegation }
}

async function callRust(from: JsNode, target: PeerId | ReturnType<typeof multiaddr>, text: string): Promise<string> {
  const stream = await from.node.dialProtocol(target, RPC_PROTOCOL, { runOnLimitedConnection: true })
  stream.send(requestFrame(from.delegation, ECHO, encode(text)))
  await stream.close()
  const response = parseResponse(await readAll(stream))
  if (!response || !(await verifyDelegation(response.delegation))) throw new Error('invalid response')
  return decode(response.payload)
}

async function readAll(stream: Stream): Promise<Uint8Array> {
  const parts: Uint8Array[] = []
  for await (const chunk of stream) parts.push(chunk instanceof Uint8Array ? chunk : chunk.subarray())
  return new Uint8Array(Buffer.concat(parts))
}

let rust: RustPeer
let js: JsNode
const extra: RustPeer[] = []
const extraJs: JsNode[] = []

beforeAll(async () => {
  rust = new RustPeer({ SEED: '21', GROUP_SEED: '121' })
  await rust.start()
  js = await startJs(22, 122)
})

afterAll(async () => {
  // js first: it closes its connections while the other end still answers.
  const started = Date.now()
  // A node whose relay is already gone can take long to stop; the test is over either way.
  await Promise.all([js, ...extraJs].map((node) => Promise.race([node?.node.stop(), Bun.sleep(5000)])))
  if (process.env.VERBOSE) console.log(`js stopped in ${Date.now() - started} ms`)
  for (const peer of [rust, ...extra]) peer?.send('quit')
  await Promise.all([rust, ...extra].map((peer) => peer?.process.exited))
}, 20_000)

test('a js-libp2p node starts under Bun with every service the mirror needs', () => {
  expect(js.node.status).toBe('started')
  const protocols = js.node.getProtocols()
  expect(protocols).toContain(DHT_PROTOCOL)
  expect(protocols).toContain(RPC_PROTOCOL)
  expect(protocols.some((p) => p.startsWith('/meshsub/'))).toBe(true)
})

test('js dials rust, and both sides identify each other', async () => {
  await js.node.dial(multiaddr(rust.address))
  const [, node] = await rust.waitFor('CONNECTED', (w) => w[1] === hex(js.key.publicKey.raw))
  expect(node).toBe(hex(js.key.publicKey.raw))
  // identify has run when rust's protocols are known on the js side.
  const deadline = Date.now() + 5000
  while (Date.now() < deadline) {
    const peer = await js.node.peerStore.get(rust.peerId).catch(() => undefined)
    if (peer?.protocols.includes(RPC_PROTOCOL)) return
    await Bun.sleep(100)
  }
  throw new Error('rust never identified itself with the RPC protocol')
})

test('js calls rust: the frame, the protocol name and both delegations are understood', async () => {
  const stream = await js.node.dialProtocol(rust.peerId, RPC_PROTOCOL)
  stream.send(requestFrame(js.delegation, ECHO, encode('ping')))
  await stream.close()
  const response = parseResponse(await readAll(stream))
  expect(response).toBeDefined()
  expect(decode(response!.payload)).toBe('rust:ping')
  const delegation = await verifyDelegation(response!.delegation)
  expect(delegation).toBeDefined()
  expect(hex(delegation!.node)).toBe(rust.key)
  expect(hex(delegation!.group)).toBe(rust.group)

  const [, , group, node, protocol, payload] = await rust.waitFor('REQUEST', (w) => w[5] === 'ping')
  expect(group).toBe(hex(js.group.publicKey.raw))
  expect(node).toBe(hex(js.key.publicKey.raw))
  expect(protocol).toBe('0')
  expect(payload).toBe('ping')
})

test('rust calls js by node: the js responder is a valid libp2p-ffi node', async () => {
  rust.send(`call ${hex(js.group.publicKey.raw)} ${hex(js.key.publicKey.raw)} hello`)
  const [, id] = await rust.waitFor('CALLED')
  const [, , group, node, payload] = await rust.waitFor('RESPONSE', (w) => w[1] === id)
  expect(group).toBe(hex(js.group.publicKey.raw))
  expect(node).toBe(hex(js.key.publicKey.raw))
  expect(payload).toBe('js:hello')
})

test('a rust announcement reaches js and carries a valid delegation', { timeout: 30_000 }, async () => {
  const pubsub = js.node.services.pubsub
  pubsub.subscribe(ANNOUNCE_TOPIC)
  const received = new Promise<{ from: string; data: Uint8Array }>((resolve) => {
    pubsub.addEventListener('message', (event: any) => {
      const msg = event.detail
      if (msg.topic === ANNOUNCE_TOPIC && msg.type === 'signed') resolve({ from: msg.from.toString(), data: msg.data })
    })
  })
  // The mesh needs a heartbeat to form.
  await Bun.sleep(2000)
  rust.send('announce hello from rust')
  await rust.waitFor('OK')
  const { from, data } = await Promise.race([
    received,
    Bun.sleep(15_000).then(() => {
      throw new Error('no announcement')
    }),
  ])
  expect(from).toBe(rust.peerId.toString())
  const frame = parseResponse(data)
  expect(decode(frame!.payload)).toBe('hello from rust')
  const delegation = await verifyDelegation(frame!.delegation)
  expect(hex(delegation!.node)).toBe(rust.key)
})

test('a js announcement reaches rust, which checks its delegation', async () => {
  await js.node.services.pubsub.publish(ANNOUNCE_TOPIC, responseFrame(js.delegation, encode('hello from js')))
  const [, group, node, ...payload] = await rust.waitFor('ANNOUNCEMENT', (w) => w[2] === hex(js.key.publicKey.raw))
  expect(group).toBe(hex(js.group.publicKey.raw))
  expect(payload.join(' ')).toBe('hello from js')
})

test('rust finds js by its group key in the DHT', { timeout: 30_000 }, async () => {
  // js provides under the key libp2p-ffi looks its groups up by.
  for await (const _ of js.node.services.dht.provide(await providerKey(js.group.publicKey.raw))) {
  }
  rust.send(`call ${hex(js.group.publicKey.raw)} - by-group`)
  const [, id] = await rust.waitFor('CALLED')
  const [kind, , , , payload] = await rust.waitFor('', (w) => (w[0] === 'RESPONSE' || w[0] === 'FAILED') && w[1] === id, 20_000)
  expect(kind).toBe('RESPONSE')
  expect(payload).toBe('js:by-group')
})

test('js finds rust by its group key in the DHT', { timeout: 30_000 }, async () => {
  const cid = await providerKey(unhex(rust.group))
  let found: PeerId | undefined
  for await (const event of js.node.services.dht.findProviders(cid)) {
    if (event.name === 'PROVIDER') {
      found = event.providers.find((p: any) => p.id.equals(rust.peerId))?.id
      if (found) break
    }
  }
  expect(found?.toString()).toBe(rust.peerId.toString())
})

test('js calls rust over QUIC', { timeout: 30_000 }, async () => {
  const quicPeer = new RustPeer({ SEED: '31', GROUP_SEED: '131', LISTEN: '/ip4/127.0.0.1/udp/0/quic-v1' })
  extra.push(quicPeer)
  await quicPeer.start()
  expect(quicPeer.address).toContain('/quic-v1/')
  expect(await callRust(js, multiaddr(quicPeer.address), 'over-quic')).toBe('rust:over-quic')
})

test('a js node without a reachable address is called through a rust relay', { timeout: 40_000 }, async () => {
  // The TypeScript community without a URL, relayed by a community on the fast path.
  const relay = new RustPeer({ SEED: '41', GROUP_SEED: '141', RELAY_SERVER: '1' })
  const caller = new RustPeer({ SEED: '42', GROUP_SEED: '142' })
  extra.push(relay, caller)
  await relay.start()
  await caller.start()
  const hidden = await startJs(43, 143, { listen: [`${relay.address}/p2p-circuit`] })
  extraJs.push(hidden)
  const deadline = Date.now() + 15_000
  let circuit: string | undefined
  while (!circuit && Date.now() < deadline) {
    circuit = hidden.node.getMultiaddrs().map((a) => a.toString()).find((a) => a.includes('/p2p-circuit'))
    if (!circuit) await Bun.sleep(200)
  }
  expect(circuit).toBeDefined()

  const hiddenKey = hex(hidden.key.publicKey.raw)
  caller.send(`add ${hiddenKey} ${relay.address}/p2p-circuit`)
  await caller.waitFor('OK')
  caller.send(`call ${hex(hidden.group.publicKey.raw)} ${hiddenKey} relayed`)
  const [, id] = await caller.waitFor('CALLED')
  const [kind, , , node, payload] = await caller.waitFor('', (w) => (w[0] === 'RESPONSE' || w[0] === 'FAILED') && w[1] === id, 20_000)
  expect(kind).toBe('RESPONSE')
  expect(node).toBe(hiddenKey)
  expect(payload).toBe('js:relayed')
  const [, , address] = await caller.waitFor('CONNECTED', (w) => w[1] === hiddenKey)
  expect(address).toContain('/p2p-circuit')
})

/** A topic key, as a caller derives one: 32 bytes standing for a shard or a community. */
const TOPIC = new Uint8Array(32).fill(0x5a)

test('a rust topic message reaches js, and a js one reaches rust', { timeout: 40_000 }, async () => {
  const pubsub = js.node.services.pubsub
  const topic = topicName(TOPIC)
  pubsub.subscribe(topic)
  rust.send(`subscribe ${hex(TOPIC)}`)
  await rust.waitFor('OK')

  const received = new Promise<{ from: string; data: Uint8Array }>((resolve) => {
    pubsub.addEventListener('message', (event: any) => {
      const msg = event.detail
      if (msg.topic === topic && msg.type === 'signed') resolve({ from: msg.from.toString(), data: msg.data })
    })
  })
  // The two are connected from the earlier tests; the mesh forms on the next heartbeat.
  await Bun.sleep(2000)
  rust.send(`peers ${hex(TOPIC)}`)
  const [, peers] = await rust.waitFor('PEERS')
  expect(Number(peers)).toBeGreaterThan(0)

  rust.send(`publish ${hex(TOPIC)} block 7`)
  await rust.waitFor('OK')
  const { from, data } = await Promise.race([
    received,
    Bun.sleep(15_000).then(() => {
      throw new Error('no topic message')
    }),
  ])
  expect(from).toBe(rust.peerId.toString())
  const frame = parseResponse(data)
  expect(decode(frame!.payload)).toBe('block 7')
  expect(hex((await verifyDelegation(frame!.delegation))!.node)).toBe(rust.key)

  // The same frame the other way: rust reports it with the topic key in front of the payload.
  await pubsub.publish(topic, responseFrame(js.delegation, encode('block 8')))
  const [, seen, group, node, ...payload] = await rust.waitFor('TOPIC', (w) => w[3] === hex(js.key.publicKey.raw))
  expect(seen).toBe(hex(TOPIC))
  expect(group).toBe(hex(js.group.publicKey.raw))
  expect(node).toBe(hex(js.key.publicKey.raw))
  expect(payload.join(' ')).toBe('block 8')

  // Left again: what is published afterwards is not reported.
  rust.send(`unsubscribe ${hex(TOPIC)}`)
  await rust.waitFor('OK')
  await Bun.sleep(2000)
  await pubsub.publish(topic, responseFrame(js.delegation, encode('block 9')))
  await Bun.sleep(2000)
  await expect(rust.waitFor('TOPIC', () => true, 1000)).rejects.toThrow()
})

test('a private rust node reserves on a js relay and is called through it', { timeout: 40_000 }, async () => {
  // A fast-path community without a URL, relayed by a TypeScript community.
  const relay = await startJs(51, 151, { relayServer: true, listen: ['/ip4/127.0.0.1/tcp/0'] })
  extraJs.push(relay)
  const relayTcp = relay.node.getMultiaddrs().map((a) => a.toString()).find((a) => a.includes('/tcp/'))!
  const hidden = new RustPeer({ SEED: '52', GROUP_SEED: '152', REACHABILITY: 'private' })
  extra.push(hidden)
  await hidden.start()
  hidden.send(`add ${hex(relay.key.publicKey.raw)} ${relayTcp.replace(/\/p2p\/[^/]+$/, '')}`)
  await hidden.waitFor('OK')
  hidden.send('bootstrap')
  const [, circuit] = await hidden.waitFor('LISTENING', (w) => w[1].includes('/p2p-circuit'), 20_000)
  expect(circuit).toContain(relay.node.peerId.toString())

  const through = multiaddr(circuit.includes(`/p2p/${hidden.peerId}`) ? circuit : `${circuit}/p2p/${hidden.peerId}`)
  expect(await callRust(js, through, 'via-js-relay')).toBe('rust:via-js-relay')
})
