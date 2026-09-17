# libp2p-ffi

[rust-libp2p](https://github.com/libp2p/rust-libp2p) behind a C interface.

A node finds the nodes of a **group** by the group's key, carries **request and response** between
nodes with failover, and reports what happens as **events** the caller polls. A group is any set of
nodes that share a key: one service running on several servers. The module holds mechanism and no
policy — which operations exist, what a payload means and what a peer class allows are the caller's.

It is built to change rarely. A network runs old releases for years, so the interface only grows:
new fields at the end of `lp2p_options`, new event types and status codes with new numbers, nothing
renamed, renumbered or reused.

First user: the network node of [gradido2](https://github.com/gradido/gradido2), where a group is a
community and a node one of its instances.

## Status

| | |
|---|---|
| start, poll, shutdown, stats | done |
| Kademlia: bootstrap, random walk, routing-table sample | done |
| every node provides under its group key | done |
| RPC to a group: provider lookup, address resolution, failover, last good node first, failed nodes last | done |
| RPC pinned to one node | done |
| delegation checked on every request and response | done |
| TCP + Noise + Yamux, QUIC | done |
| circuit relay v2: a PRIVATE node reserves on up to two relays and announces only relayed addresses; a node that is not PRIVATE relays for others, with every libp2p limit configurable | done |
| DCUtR: a relayed connection is upgraded by hole punching, reported as `LP2P_EV_HOLE_PUNCH` | done — verified through NAT in Docker (`interop/holepunch`): TCP and QUIC upgrade through cone NAT, and stay relayed through symmetric NAT. With js-libp2p nodes it does not upgrade; see below |
| connection limits: total, per peer, pending incoming | done |
| AutoNAT: a node configured UNKNOWN is moved to PUBLIC or PRIVATE by dial-back probes, and reserves on relays when it turns out private | done — verified on loopback with `--features test-loopback`; the move back from PRIVATE to PUBLIC is implemented but not tested |
| peer classes per group, blocked groups, token-bucket limits per class, scope (peer, IP prefix, global) and protocol | done — checked once a request and its delegation have arrived |
| announcement over gossipsub: signed, published on change, delegation checked before it is reported or forwarded, blocked groups dropped, at most one per ten seconds per node (three in reserve) | done |
| topics: subscribe, publish, `lp2p_topic_peers`, and a mesh bootstrapped over the DHT -- providing the topic key on subscribe, looking it up, dialing a few members, and repeating while a topic has no peer | done -- gossipsub never dials to fill a mesh, so a topic with a handful of members needs this to work at all |
| byte-rate limits beside the message rates, per class, scope and protocol, with `LP2P_PROTOCOL_TOPICS` for published messages | done -- the two hold at once; whichever runs out first stops the message |
| provider records under any key the caller chooses: `lp2p_dht_provide`, `lp2p_dht_stop_providing`, `lp2p_dht_find_providers` | done |
| interop with js-libp2p under Bun: RPC, announcements, topics, DHT lookups and relay in both directions, QUIC, and `bun build --compile` | done — `interop/js`; DCUtR and AutoNAT across the two in `interop/holepunch`, with the limits of js-libp2p written down below |

## Layout

```text
include/libp2p_ffi.h   the interface -- the one file a C caller reads
src/ffi.rs             the extern "C" functions; the only module allowed unsafe
src/node.rs            the swarm on its own tokio runtime, commands in, events out
src/wire.rs            the RPC frames, byte for byte
src/delegation.rs      "node X belongs to group Y until T", signed by the group key
src/events.rs          the bounded event queue and the record format
src/address_book.rs    addresses of peers the routing table does not hold
src/keys.rs            32-byte ed25519 keys <-> peer ids
src/limits.rs          peer classes and token buckets
scripts/localize.sh    release build -> dist/<target>/ the object, .h, NATIVE_LIBS.txt, SHA256SUMS
scripts/release-version.sh  what makes a merge a release, for the workflows and for a local check
scripts/c-smoke.sh     links tests/c/smoke.c against that object with cc and zig cc
examples/holepunch.rs  one node of the NAT test, by ROLE
interop/holepunch/     two nodes behind NAT routers and a relay, in Docker
examples/interop_peer.rs  a node driven over stdin, the counterpart for tests in other languages
interop/js/            js-libp2p under Bun against this module
tests/abi_layout.rs    the C compiler's layout of the header against the Rust one
tests/network.rs       nodes on loopback, driven through the C interface: group calls with
                       failover, a private node reached through a relay, with and without DCUtR,
                       limits per class and a blocked group, announcements, topics through a
                       node the publisher cannot dial, and two members that find each other
                       through nothing but the DHT
```

## Build and test

```sh
cargo test                  # unit tests, ABI layout (needs a C compiler), network on loopback
cargo test --features test-loopback   # also AutoNAT, which needs loopback addresses accepted
interop/holepunch/run.sh              # hole punching through NAT, in Docker, no root needed
interop/js/run.sh                     # js-libp2p under Bun against this module
scripts/localize.sh         # dist/host/libp2p_ffi.o
scripts/c-smoke.sh          # the shipped object, linked from C and run
```

The toolchain is pinned in `rust-toolchain.toml`, and libp2p to an exact version in `Cargo.toml`:
a libp2p upgrade is a deliberate release, not a lockfile update.

## NAT: hole punching and AutoNAT, Rust and js-libp2p

Loopback has no NAT, so neither hole punching nor AutoNAT can be tested there. `interop/holepunch`
builds one:

```text
lan-a 10.99.1.0/24          public 11.99.0.0/24          lan-b 10.99.2.0/24
dialer .10 -- router-a .2 | .11 -- relay .10 -- .12 | .2 router-b -- .10 listener
client-nat .20 -/            client-public .13
```

The routers are Debian containers with `iptables`: MASQUERADE for cone NAT, `--random-fully` for
symmetric NAT, nothing forwarded in that the LAN did not ask for, and unsolicited packets to the
router dropped rather than answered. The "public" network uses 11.99.0.0/24 because js-libp2p's
DCUtR and AutoNAT, and rust-libp2p's AutoNAT, ignore private addresses by design; all networks are
internal, so nothing leaves the host whatever the range. Every role can be played by the Rust
`holepunch` example or by `interop/js/holepunch-node.ts` compiled with Bun -- same seeds, same peer
ids, same RESULT line.

```sh
interop/holepunch/run.sh                                  # everything below
interop/holepunch/run.sh holepunch rust-js quic cone      # one hole-punching combination
interop/holepunch/run.sh autonat js-rust tcp              # one AutoNAT combination
```

`VERBOSE=1` prints every container's log, `LP2P_TRACE="libp2p_dcutr=debug"` adds rust-libp2p's
tracing. Every line says `as-expected` or `UNEXPECTED`, and the expectations in `run.sh` carry the
reason for every combination that is expected to fail. It runs on rootless Docker; the routers need
`NET_ADMIN`, which Docker grants inside the container without root on the host.

### Hole punching (dialer-listener, relay always Rust)

```text
                 cone NAT        symmetric NAT
rust-rust tcp    direct          relayed
rust-rust quic   direct          relayed
any pair with js relayed         relayed     -- js offers no addresses, see below
```

The call gets its answer in every combination; what differs is whether the connection is upgraded.

**js-libp2p does not hole punch from behind NAT.** Its DCUtR offers the peer only addresses its
AutoNAT has verified, and behind NAT AutoNAT verifies none: the dial-back is exactly what the NAT
drops. With `TRUST_OBSERVED=1` the js nodes confirm their observed addresses themselves, as
rust-libp2p offers them, and one combination gets through -- a js dialer to a Rust listener over QUIC.
The others still cannot: js-libp2p's QUIC dials each connection from a new UDP socket, so the port a
peer observed is not the one it could punch to, and its TCP cannot reuse the listen port for a dial.

### AutoNAT (client-server)

```text
                      client public         client behind NAT
rust client, rust     public                private
js client, rust       address verified      unverified   -- js has no private verdict
any client, js server no verdict           no verdict   -- js server bug, see below
```

A Rust server answers both implementations. **js-libp2p's AutoNAT server answers nobody:** it dials
back with `openConnection` without `force`, which returns the client's existing connection; the
address check then fails and its `finally` closes that connection, request stream and all. The
client sees an unexpected end of file. And js-libp2p's client has no private verdict at all: an
observed address is verified after 4 successful dial-backs or dropped after 8 failed ones, each from a
different /8 network.

### What the tests found in the module

Fixed: a node listening on `0.0.0.0` dialed before it knew its interface addresses, so the first
connections left from random ports and peers punched towards mappings that did not lead back --
`lp2p_start` now waits for them. And a router that answers an early SYN with a reset kills the
punch, which is why the test routers drop it, as real ones do.

## What ships: a localized object

A caller links `libp2p_ffi.o`, a single relocatable object in which only the `lp2p_` functions are
global. Not the plain `staticlib`, because two Rust staticlibs in one binary break in two measured
ways:

- built with different rustc versions they collide on `rust_eh_personality`, with GNU ld and lld alike;
- a `#[global_allocator]` in one of them either fails the link or silently takes over the other's
  allocations.

`scripts/localize.sh` does the conversion: members extracted one by one, `ld -r` into one object,
`objcopy --keep-global-symbols -R .group -R .llvmbc -R .llvmcmd`, `strip --strip-unneeded`.
`-R .group` is required, or lld refuses the link over a discarded `DW.ref.rust_eh_personality`.

Release profile: fat LTO, `codegen-units = 1`, `opt-level = "s"`, `panic = "unwind"`. `panic` must
stay `"unwind"`: every exported function catches a panic and answers `LP2P_ERR_PANIC`, and with
`"abort"` a panic would end the host process instead.

Size, measured on x86_64 Linux: the object is ~30 MB of per-function sections; `tests/c/smoke.c`
linked against it with `--gc-sections` and stripped is 6.05 MB.

## The wire

A mirror in another language has to follow `src/wire.rs` and `src/delegation.rs` byte for byte. In
short: one libp2p protocol, `/lp2p/rpc/1`, one request and one response per stream, each side
closing its write half after its frame.

```text
request    u8 1 | delegation (136) | u8 name length | protocol name | payload
response   u8 1 | delegation (136) | payload
delegation node key (32) | group key (32) | expires_ms, big endian (8, 0 = never)
           | ed25519 signature by the group key over
             "libp2p-ffi delegation v1" || node key || group key || expires_ms
```

Every node announces itself as a Kademlia provider under `0x12 0x20 || sha256(group key)` -- the
multihash of `CIDv1(raw, sha2-256(group key))`, because js-libp2p's DHT names keys by CID only. A
stream closed without a response is a rejection. Every node runs `/ipfs/ping/1.0.0`: a js-libp2p DHT
pings a peer before it adds it to its routing table.

```text
announcement   u8 1 | delegation (136) | payload
               as the data of a gossipsub message the node signs (strict validation), on the
               topic "<dht_protocol>/announce" unless the caller names another
topic message  the same frame, on "/lp2p/topic/1/" + the 32-byte topic key in lowercase hex
               reported with the topic key in front of the payload; the topic key is also
               provided in the DHT under 0x12 0x20 || sha256(topic key), which is how the
               members of a small topic find each other
```

## Releases

A release is a pull request whose **title says "release"** and whose **`Cargo.toml` version has
not been released before**: not tagged yet, after every tag, and -- when the title names a version
-- the same one the file says. The title is what makes it deliberate; the tags are what make it
safe. A version that is unchanged on the branch is fine and says so in the log: bumping in one
pull request and releasing in another is ordinary, and a first release has nothing to bump from.

[`CHANGELOG.md`](CHANGELOG.md) says per version what moved in the ABI, on the wire and in the
build. The generated release notes list the pull requests; that file answers whether a caller
still compiles and whether old nodes still understand the new ones.

`scripts/release-version.sh` is that rule, and `.github/workflows` runs it twice -- on the open
pull request, so a mismatch is a red check rather than a surprise, and again at merge, because a
title can be edited after a green one. **Make the check required on the default branch**
(Settings -> Rules, require the status check named *release version*): without that, GitHub shows
the red mark and lets the merge through anyway.

What a merge then builds, on a native runner each, is one archive per target:

```text
x86_64-unknown-linux-gnu    aarch64-unknown-linux-gnu     libp2p_ffi.o
x86_64-apple-darwin         aarch64-apple-darwin          libp2p_ffi.o
x86_64-pc-windows-msvc      aarch64-pc-windows-msvc       libp2p_ffi.lib

each archive holds  the object, libp2p_ffi.h, NATIVE_LIBS.txt (what the caller's link line
                    needs, printed by rustc rather than written down), SHA256SUMS, and on
                    Windows the one import library that comes from a crate rather than from
                    the SDK -- put the archive's directory on the library search path there
the release holds   the archives and one SHA256SUMS over them
```

Every job runs the unit tests, builds the artifact and links `tests/c/smoke.c` against it before
anything is published; the Linux jobs run the whole suite. **Windows ships the staticlib**, not a
localized object: MSVC's toolchain has no partial link. The clash that localization avoids is
ELF's, so this is expected to be fine, and it is untested with a second Rust staticlib in one
binary -- `scripts/localize.sh` says so at the point where it matters. **macOS exports one symbol
besides `lp2p_*`: `_rust_eh_personality`, as a weak definition.** `compiler_builtins`, which is
never part of LTO, refers to it from outside, and ld64 applies an export list inside the partial
link, where hiding the definition leaves those references unbound. Weak keeps a second Rust
staticlib in the same binary linkable.

## License

Apache-2.0
