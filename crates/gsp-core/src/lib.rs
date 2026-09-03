//! Core data plane for the game server proxy: the config snapshot, backend
//! pools with health state, the TCP listener accept loop, the byte pump, and
//! the active health checker.
//!
//! Roadmap status (`docs/08-roadmap.md`): phase 1 complete — TCP forwarding,
//! round-robin / least-connections balancing, per-backend session caps,
//! active `tcp_connect` health checks with passive failure feedback, and hot
//! reload via an atomic snapshot swap. UDP is phase 2.

pub mod health;
pub mod listener;
pub mod metrics_defs;
pub mod net;
pub mod pool;
pub mod proxy;
pub mod runtime;
pub mod snapshot;
mod util;

pub use runtime::{Runtime, RuntimeHandle};
pub use snapshot::Snapshot;
