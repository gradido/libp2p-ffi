//! The header and the Rust definitions describe the same bytes. The C compiler lays the header
//! out, and every size and offset it reports has to match `abi.rs`.
//!
//! Needs a C compiler: `$CC`, else `cc`. Without one the test says so and passes, because the
//! layout it guards cannot have changed without one either on the machine that changed it.

use std::collections::HashMap;
use std::mem::{offset_of, size_of};
use std::process::Command;

use libp2p_ffi::abi::*;

fn c_layout() -> Option<HashMap<String, usize>> {
    let root = env!("CARGO_MANIFEST_DIR");
    let out = std::env::temp_dir().join(format!("lp2p_layout_{}", std::process::id()));
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
    let status = Command::new(&cc)
        .args(["-std=c11", "-Wall", "-Werror", "-I"])
        .arg(format!("{root}/include"))
        .arg(format!("{root}/tests/c/layout.c"))
        .arg("-o")
        .arg(&out)
        .status()
        .ok()?;
    assert!(status.success(), "{cc} could not compile tests/c/layout.c");
    let output = Command::new(&out).output().expect("run layout");
    let _ = std::fs::remove_file(&out);
    Some(
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| {
                let (name, value) = line.split_once(' ').unwrap();
                (name.to_owned(), value.parse().unwrap())
            })
            .collect(),
    )
}

#[test]
fn rust_and_c_agree_on_every_layout() {
    let Some(c) = c_layout() else {
        eprintln!("no C compiler found; layout not checked");
        return;
    };
    let rust: Vec<(&str, usize)> = vec![
        ("sizeof.lp2p_rate", size_of::<lp2p_rate>()),
        ("sizeof.lp2p_relay_options", size_of::<lp2p_relay_options>()),
        (
            "offsetof.lp2p_relay_options.max_circuit_bytes",
            offset_of!(lp2p_relay_options, max_circuit_bytes),
        ),
        (
            "offsetof.lp2p_relay_options.circuits_per_ip",
            offset_of!(lp2p_relay_options, circuits_per_ip),
        ),
        ("sizeof.lp2p_announce_options", size_of::<lp2p_announce_options>()),
        (
            "offsetof.lp2p_announce_options.max_payload_bytes",
            offset_of!(lp2p_announce_options, max_payload_bytes),
        ),
        ("sizeof.lp2p_options", size_of::<lp2p_options>()),
        (
            "offsetof.lp2p_options.node_seed",
            offset_of!(lp2p_options, node_seed),
        ),
        (
            "offsetof.lp2p_options.delegation",
            offset_of!(lp2p_options, delegation),
        ),
        ("offsetof.lp2p_options.group", offset_of!(lp2p_options, group)),
        (
            "offsetof.lp2p_options.listen_addrs",
            offset_of!(lp2p_options, listen_addrs),
        ),
        (
            "offsetof.lp2p_options.dht_protocol",
            offset_of!(lp2p_options, dht_protocol),
        ),
        (
            "offsetof.lp2p_options.rpc_protocols",
            offset_of!(lp2p_options, rpc_protocols),
        ),
        (
            "offsetof.lp2p_options.rpc_max_request_bytes",
            offset_of!(lp2p_options, rpc_max_request_bytes),
        ),
        ("offsetof.lp2p_options.quic", offset_of!(lp2p_options, quic)),
        ("offsetof.lp2p_options.autonat", offset_of!(lp2p_options, autonat)),
        (
            "offsetof.lp2p_options.max_connections",
            offset_of!(lp2p_options, max_connections),
        ),
        ("offsetof.lp2p_options.relay", offset_of!(lp2p_options, relay)),
        (
            "offsetof.lp2p_options.announce",
            offset_of!(lp2p_options, announce),
        ),
        (
            "offsetof.lp2p_options.event_queue_bytes",
            offset_of!(lp2p_options, event_queue_bytes),
        ),
        ("sizeof.lp2p_event", size_of::<lp2p_event>()),
        ("offsetof.lp2p_event.id", offset_of!(lp2p_event, id)),
        ("offsetof.lp2p_event.group", offset_of!(lp2p_event, group)),
        ("offsetof.lp2p_event.node", offset_of!(lp2p_event, node)),
        ("offsetof.lp2p_event.protocol", offset_of!(lp2p_event, protocol)),
        ("offsetof.lp2p_event.data_len", offset_of!(lp2p_event, data_len)),
        ("sizeof.lp2p_stats", size_of::<lp2p_stats>()),
        ("offsetof.lp2p_stats.rpc_in", offset_of!(lp2p_stats, rpc_in)),
        (
            "offsetof.lp2p_stats.events_dropped",
            offset_of!(lp2p_stats, events_dropped),
        ),
        ("value.LP2P_DELEGATION_BYTES", LP2P_DELEGATION_BYTES),
        ("value.LP2P_ABI_VERSION", LP2P_ABI_VERSION as usize),
    ];
    assert_eq!(
        rust.len(),
        c.len(),
        "tests/c/layout.c and this list name different things"
    );
    for (name, value) in rust {
        assert_eq!(c.get(name), Some(&value), "{name}");
    }
}
