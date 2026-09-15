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
| DCUtR: a relayed connection is upgraded by hole punching | wired, **not working yet**: on loopback the attempt fails with `NoAddresses` because the calling side has no observed-address candidate. Calls still succeed over the relay. Needs a look with a real NAT |
| connection limits: total, per peer, pending incoming | done |
| AutoNAT | not yet — `reachability` is configured; UNKNOWN behaves as PUBLIC |
| peer classes per group, blocked groups, token-bucket limits per class, scope (peer, IP prefix, global) and protocol | done — checked once a request and its delegation have arrived |
| announcement over gossipsub: signed, published on change, delegation checked before it is reported or forwarded, blocked groups dropped, at most one per ten seconds per node (three in reserve) | done |
| interop test against js-libp2p | not yet |

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
scripts/localize.sh    release build -> dist/<target>/libp2p_ffi.o, .h, SHA256SUMS
scripts/c-smoke.sh     links tests/c/smoke.c against that object with cc and zig cc
tests/abi_layout.rs    the C compiler's layout of the header against the Rust one
tests/network.rs       nodes on loopback, driven through the C interface: group calls with
                       failover, a private node reached through a relay, with and without DCUtR,
                       limits per class and a blocked group, announcements
```

## Build and test

```sh
cargo test                  # unit tests, ABI layout (needs a C compiler), network on loopback
scripts/localize.sh         # dist/host/libp2p_ffi.o
scripts/c-smoke.sh          # the shipped object, linked from C and run
```

The toolchain is pinned in `rust-toolchain.toml`, and libp2p to an exact version in `Cargo.toml`:
a libp2p upgrade is a deliberate release, not a lockfile update.

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

Every node announces itself as a Kademlia provider under its group key (the raw 32 bytes). A
stream closed without a response is a rejection.

```text
announcement   u8 1 | delegation (136) | payload
               as the data of a gossipsub message the node signs (strict validation), on the
               topic "<dht_protocol>/announce" unless the caller names another
```

## License

Apache-2.0
