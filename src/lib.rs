//! rust-libp2p behind a C interface.
//!
//! `include/libp2p_ffi.h` is the interface; this crate implements it. The module holds mechanism
//! and no policy: it finds the nodes of a group by the group's key, carries request and response
//! between nodes with failover, and reports what happens as events a caller polls. Which
//! operations exist, what a payload means and what a peer class allows are the caller's.
//!
//! Safe Rust ends at the `extern "C"` line. All `unsafe` lives in [`ffi`], the one module allowed
//! to have it; everything else is denied it.
#![deny(unsafe_code)]

pub mod abi;
mod address_book;
pub mod delegation;
mod events;
#[allow(unsafe_code)]
pub mod ffi;
mod keys;
mod node;
mod wire;
