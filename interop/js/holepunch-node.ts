/**
 * The js-libp2p counterpart of examples/holepunch.rs, for interop/holepunch: the same roles, the
 * same seeds -- so a peer id does not depend on which implementation plays a role -- and the same
 * RESULT line and exit codes. build-holepunch.ts compiles it into one executable.
 *
 *   ROLE=relay            relays for the others and answers their AutoNAT probes
 *   ROLE=listener         behind NAT b: reserves on the relay, answers every call with "pong"
 *   ROLE=dialer           behind NAT a: calls the listener through the relay, waits for DCUtR to
 *                         open a direct connection; exits 0 direct, 1 still relayed, 2 no answer
 *   ROLE=autonat-client   asks the relay to verify its address; exits 0 once one is verified,
 *                         2 when none is within a minute
 *   TRANSPORT=tcp|quic    RELAY_IP=11.99.0.10    SEED=<byte> for an autonat client
 */
import { noise } from '@chainsafe/libp2p-noise'
import { quic } from '@chainsafe/libp2p-quic'
import { yamux } from '@chainsafe/libp2p-yamux'
import { autoNAT } from '@libp2p/autonat'
import { circuitRelayServer, circuitRelayTransport } from '@libp2p/circuit-relay-v2'
import { dcutr } from '@libp2p/dcutr'
import { identify } from '@libp2p/identify'
import type { Connection, PeerId, Stream } from '@libp2p/interface'
import { isPrivate } from '@libp2p/utils'
import { peerIdFromPrivateKey } from '@libp2p/peer-id'
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
  requestFrame,
  responseFrame,
  signDelegation,
  verifyDelegation,
} from './lp2p.ts'

const PORT = 4001
const role = process.env.ROLE ?? 'dialer'
const transport = process.env.TRANSPORT ?? 'tcp'
const relayIp = process.env.RELAY_IP ?? '11.99.0.10'
const say = (line: string) => console.log(`${role}: ${line}`)
const trustObserved = process.env.TRUST_OBSERVED === '1'

const address = (ip: string) => (transport === 'quic' ? `/ip4/${ip}/udp/${PORT}/quic-v1` : `/ip4/${ip}/tcp/${PORT}`)

async function peerOf(seed: number): Promise<PeerId> {
  return peerIdFromPrivateKey(await keyFromSeed(seed))
}

async function readAll(stream: Stream): Promise<Uint8Array> {
  const parts: Uint8Array[] = []
  for await (const chunk of stream) parts.push(chunk instanceof Uint8Array ? chunk : chunk.subarray())
  return new Uint8Array(Buffer.concat(parts))
}

async function start(seed: number, groupSeed: number, listen: string[], relayServer: boolean) {
  const key = await keyFromSeed(seed)
  const group = await keyFromSeed(groupSeed)
  const delegation = await signDelegation(group, key.publicKey.raw)
  const services: Record<string, any> = {
    identify: identify(),
    ping: ping(),
    autonat: autoNAT(),
    dcutr: dcutr(),
  }
  if (relayServer) services.relay = circuitRelayServer()
  const node: Libp2p<any> = await createLibp2p({
    privateKey: key,
    addresses: { listen },
    transports: [tcp(), quic(), circuitRelayTransport()],
    connectionEncrypters: [noise()],
    streamMuxers: [yamux()],
    services,
  })
  node.addEventListener('connection:open', (event: CustomEvent<Connection>) => {
    say(`connected ${event.detail.remotePeer} over ${event.detail.remoteAddr}`)
  })
  if (trustObserved) {
    // js-libp2p's DCUtR offers only verified addresses, and behind NAT AutoNAT never verifies the
    // observed one -- the dial-back is exactly what the NAT drops. rust-libp2p offers observed
    // addresses unverified. TRUST_OBSERVED=1 does the same here, by confirming every public
    // address a peer reports having seen.
    const addressManager = (node as any).components.addressManager
    const confirm = () => {
      for (const ma of addressManager.getObservedAddrs()) {
        if (!isPrivate(ma)) {
          addressManager.confirmObservedAddr(ma)
          say(`trusting observed ${ma}`)
        }
      }
    }
    node.addEventListener('peer:identify', () => setTimeout(confirm, 0))
  }
  return { node, delegation }
}

const relayPeer = await peerOf(1)
const relayAddress = `${address(relayIp)}/p2p/${relayPeer}`

if (role === 'relay') {
  const { node } = await start(1, 0x11, [address('0.0.0.0')], true)
  say(`peer ${node.peerId}`)
} else if (role === 'listener') {
  const { node, delegation } = await start(2, 0x22, [address('0.0.0.0'), `${relayAddress}/p2p-circuit`], false)
  await node.handle(
    RPC_PROTOCOL,
    async (stream: Stream, connection: Connection) => {
      const request = parseRequest(await readAll(stream))
      const checked = request && (await verifyDelegation(request.delegation))
      if (!request || !checked || hex(checked.node) !== hex(connection.remotePeer.publicKey!.raw)) {
        stream.abort(new Error('refused'))
        return
      }
      stream.send(responseFrame(delegation, new TextEncoder().encode('pong')))
      await stream.close()
    },
    { runOnLimitedConnection: true },
  )
  node.addEventListener('self:peer:update', () => {
    for (const ma of node.getMultiaddrs()) if (ma.toString().includes('/p2p-circuit')) say(`listening ${ma}`)
  })
} else if (role === 'autonat-client') {
  const seed = Number(process.env.SEED ?? 4)
  const { node } = await start(seed, (seed + 0x40) & 0xff, [address('0.0.0.0')], false)
  await node.dial(multiaddr(relayAddress))
  const deadline = Date.now() + 60_000
  let verified: string | undefined
  while (!verified && Date.now() < deadline) {
    const addresses = (node as any).components.addressManager.getAddressesWithMetadata() as Array<{ multiaddr: any; verified: boolean }>
    verified = addresses.find((a) => a.verified && !isPrivate(a.multiaddr))?.multiaddr.toString()
    if (!verified) await Bun.sleep(500)
  }
  say(`RESULT impl=js transport=${transport} reachability=${verified ? `public address=${verified}` : 'unknown'}`)
  await Promise.race([node.stop(), Bun.sleep(2000)])
  process.exit(verified ? 0 : 2)
} else {
  const { node, delegation } = await start(3, 0x33, [address('0.0.0.0')], false)
  const listenerPeer = await peerOf(2)
  const circuit = multiaddr(`${relayAddress}/p2p-circuit/p2p/${listenerPeer}`)
  const direct = () => node.getConnections(listenerPeer).find((c) => !c.remoteAddr.toString().includes('/p2p-circuit'))

  let answered = false
  const deadline = Date.now() + 60_000
  while (!answered && Date.now() < deadline) {
    try {
      const stream = await node.dialProtocol(circuit, RPC_PROTOCOL, { runOnLimitedConnection: true })
      stream.send(requestFrame(delegation, '/holepunch/echo/1', new TextEncoder().encode('ping')))
      await stream.close()
      const response = parseResponse(await readAll(stream))
      answered = response != null && (await verifyDelegation(response.delegation)) != null
    } catch (error) {
      say(`call failed: ${error}`)
      await Bun.sleep(2000)
    }
  }
  if (!answered) {
    say(`RESULT impl=js transport=${transport} rpc=failed`)
    process.exit(2)
  }
  const firstRelayed = node.getConnections(listenerPeer).some((c) => c.remoteAddr.toString().includes('/p2p-circuit'))
  const punchDeadline = Date.now() + 30_000
  while (!direct() && Date.now() < punchDeadline) await Bun.sleep(250)
  const upgraded = direct()
  say(
    `RESULT impl=js transport=${transport} rpc=ok first_connection_relayed=${firstRelayed} ` +
      (upgraded ? `hole_punch=direct address=${upgraded.remoteAddr}` : 'hole_punch=none'),
  )
  await Promise.race([node.stop(), Bun.sleep(2000)])
  process.exit(upgraded ? 0 : 1)
}
