# Changelog

What somebody who pinned an earlier prebuild has to know before pinning the next one. The
generated release notes list the pull requests; this file answers the two questions a list of pull
requests does not: **does my C caller still compile, and do old nodes still understand the new
ones?**

A network runs mixed versions for years, so every entry says which of the three it touched:

```text
ABI    the C header. Only grows: fields at the end, new numbers for new events and options.
       LP2P_ABI_VERSION moves only when that promise is broken, which is not planned.
wire   what travels between nodes. A change here is a change every implementation has to make,
       including the js-libp2p mirror.
build  what the prebuild archive holds and what the caller's link line needs.
```

## 0.1.2

The first published release. 0.1.0 and 0.1.1 were tagged by nobody: the pipeline that builds a
release was still being taught its own platforms, and a version it could not build was left
behind rather than reused.

- **ABI** `LP2P_ABI_VERSION` 1. Node lifecycle, event polling, RPC to a group with failover,
  peer classes and token-bucket limits in messages and bytes, relay and reachability options,
  topics, provider records under any key, keys and delegations.
- **wire** RPC frame `1 | delegation (136) | name length | protocol name | payload`, response and
  announcement `1 | delegation (136) | payload`, delegation signed over
  `"libp2p-ffi delegation v1" || node key || group key || expires_ms`. A group is provided under
  `0x12 0x20 || sha256(group key)`, a topic is `"/lp2p/topic/1/" + topic key in lowercase hex` and
  is provided under the same multihash shape. `/lp2p/rpc/1` carries every call, `/ipfs/ping/1.0.0`
  is answered because a js-libp2p DHT pings before it keeps a peer. rust-libp2p 0.56, pinned
  exactly.
- **build** One archive per target: the object (`libp2p_ffi.o`; `libp2p_ffi.lib` on Windows, where
  MSVC has no partial link), `libp2p_ffi.h`, `NATIVE_LIBS.txt` with what the link line needs, and
  `SHA256SUMS`. Linux, macOS and Windows, x64 and arm64.
  **On Windows the archive's own directory belongs on the library search path**
  (`/LIBPATH:<archive>`): `NATIVE_LIBS.txt` names an import library that comes from a crate rather
  than from the Windows SDK, and it travels in the archive.
  The Intel macOS object is cross-built on an arm64 runner — GitHub has retired the Intel ones —
  so it is linked but never executed before release; every other artifact runs its smoke test.
