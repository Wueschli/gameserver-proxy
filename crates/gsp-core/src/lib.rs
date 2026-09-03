//! Core data plane for the game server proxy: the config snapshot, backend
//! pools, the TCP listener accept loop, and the byte pump.
//!
//! The scaffold implements the **walking skeleton** from
//! `docs/08-roadmap.md` phase 1, slice 1: a TCP listener forwarding to a
//! static round-robin pool. UDP, routing matchers, health checks and hot
//! reload are added in later slices.

pub mod listener;
pub mod metrics_defs;
pub mod net;
pub mod pool;
pub mod proxy;
pub mod runtime;
pub mod snapshot;

pub use runtime::{Runtime, RuntimeHandle};
pub use snapshot::Snapshot;
