# interop/js

js-libp2p under Bun against libp2p-ffi. This is the check that a TypeScript node can be the same
node as the Rust module -- the mirror gradido2's TypeScript path needs -- and that it survives
`bun build --compile`.

```sh
interop/js/run.sh
```

## What is tested

```text
interop.test.ts        a js node and examples/interop_peer on loopback
  start                js-libp2p starts under Bun with TCP, QUIC, relay transport, identify,
                       ping, Kademlia and gossipsub
  identify             both sides dial and identify each other
  RPC, both ways       /lp2p/rpc/1 frames, protocol names and delegations, checked on both sides
  announcements        a signed gossipsub announcement in each direction, delegation checked
  DHT, both ways       each side finds the other's group by its provider key
  QUIC                 js calls rust over QUIC
  relay, both ways     a js node without a reachable address called through a rust relay, and a
                       private rust node that reserved on a js relay called through it
compile-check.ts       a node with every service, including QUIC's native binding and DCUtR and
                       AutoNAT, built with `bun build --compile` and run from /tmp
```

DCUtR and AutoNAT across the two need NAT and run in `interop/holepunch`, where every role can be
played by `holepunch-node.ts` compiled with `build-holepunch.ts`. The results, in short: calls through
a relay work in every pairing; js-libp2p nodes do not hole punch from behind NAT; a Rust AutoNAT
server answers js clients correctly, and a js AutoNAT server answers no client at all. The
repository README has the tables and the reasons.

## What a TypeScript mirror has to do

Each of these was found by these tests, and each breaks interop silently when it is missing:

- **`lp2p.ts` is the wire.** Delegation, request and response frames, and the provider key are
  written out there and must stay byte for byte equal to `src/delegation.rs` and `src/wire.rs`.
- **The provider key is a CID.** js-libp2p's DHT provides and looks up by CID only, so a group is
  provided under `CIDv1(raw, sha2-256(group key))`, whose multihash is the module's key.
- **Run `@libp2p/ping`.** kad-dht requires it, and pings every new peer before it adds it to its
  routing table. (It is also why the Rust module runs ping: without it, a js DHT never kept a Rust
  node.)
- **`peerInfoMapper: passthroughMapper`** only for loopback tests; the default drops private
  addresses, which is right on the internet.
- **`runOnLimitedConnection: true`** on the RPC handler and on every RPC dial. js-libp2p refuses
  streams on relayed connections otherwise, and a community without a URL is only ever reached
  over one.
- **Build with `quic-binding-plugin.ts`.** `@chainsafe/libp2p-quic` loads its native binding at run
  time in a way the bundler cannot follow; the plugin replaces that loader with a static require,
  which `--compile` embeds.

- **Configure reachability, do not expect AutoNAT to find it.** js-libp2p's client never reaches a
  private verdict and its server answers nobody (see the README). A TypeScript community with a URL
  is public by configuration; one without is private, reserves on relays and is called over them.
- **Do not count on hole punching.** Behind NAT js-libp2p offers no addresses for it; the relay is
  the path, and it works.

Versions are pinned exactly in `package.json`, and `bun.lock` is committed. A version is raised
only with this test green.
