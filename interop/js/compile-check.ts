/**
 * Does a js-libp2p node with every service the mirror needs survive `bun build --compile`?
 * gradido2 ships as one executable, so a node that only runs under `bun run` is not enough.
 *
 * Starts a node with TCP, QUIC (a native module), relay client and server, DCUtR, AutoNAT,
 * identify, ping, Kademlia and gossipsub, has it dial a second node over QUIC and TCP, and exits
 * 0 when both connections came up.
 */
import { noise } from '@chainsafe/libp2p-noise'
import { quic } from '@chainsafe/libp2p-quic'
import { yamux } from '@chainsafe/libp2p-yamux'
import { autoNAT } from '@libp2p/autonat'
import { circuitRelayServer, circuitRelayTransport } from '@libp2p/circuit-relay-v2'
import { dcutr } from '@libp2p/dcutr'
import { gossipsub } from '@libp2p/gossipsub'
import { identify } from '@libp2p/identify'
import { kadDHT, passthroughMapper } from '@libp2p/kad-dht'
import { ping } from '@libp2p/ping'
import { tcp } from '@libp2p/tcp'
import { createLibp2p } from 'libp2p'

async function start() {
  return createLibp2p({
    addresses: { listen: ['/ip4/127.0.0.1/tcp/0', '/ip4/127.0.0.1/udp/0/quic-v1'] },
    transports: [tcp(), quic(), circuitRelayTransport()],
    connectionEncrypters: [noise()],
    streamMuxers: [yamux()],
    services: {
      identify: identify(),
      ping: ping(),
      autonat: autoNAT(),
      dcutr: dcutr(),
      relay: circuitRelayServer(),
      dht: kadDHT({ protocol: '/check/kad/1', clientMode: false, peerInfoMapper: passthroughMapper }),
      pubsub: gossipsub(),
    },
  })
}

const a = await start()
const nodes = [a]
let ok = true
for (const kind of ['quic-v1', 'tcp']) {
  // A peer per transport: two pings to one peer would share its connection.
  const b = await start()
  nodes.push(b)
  const address = b.getMultiaddrs().find((m) => m.toString().includes(`/${kind}`))!
  try {
    const rtt = await a.services.ping.ping(address)
    console.log(`${kind}: connected, ping ${rtt} ms`)
  } catch (error) {
    ok = false
    console.log(`${kind}: FAILED ${error}`)
  }
}
await Promise.race([Promise.all(nodes.map((n) => n.stop())), Bun.sleep(3000)])
console.log(ok ? 'compile check: ok' : 'compile check: FAILED')
process.exit(ok ? 0 : 1)
