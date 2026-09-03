//! Core data plane for the game server proxy: the config snapshot, backend
//! pools with health state, the TCP listener accept loop, the byte pump, and
//! the active health checker.
//!
//! Roadmap status (`docs/08-roadmap.md`): phases 1–2 — TCP and UDP forwarding,
//! round-robin / least-connections balancing, per-backend session caps,
//! active `tcp_connect` / `udp_probe` health checks with passive failure
//! feedback, per-worker UDP session tables with `src_ip` affinity, and hot
//! reload via an atomic snapshot swap.

pub mod health;
pub mod listener;
pub mod listener_udp;
pub mod metrics_defs;
pub mod net;
pub mod pool;
pub mod proxy;
pub mod resolver;
pub mod route_hint;
pub mod runtime;
pub mod snapshot;
pub mod sniff;
mod util;

pub use resolver::{Resolution, ResolveError, ResolveRequest, Resolver, Resolvers};
pub use route_hint::RouteHints;
pub use runtime::{Runtime, RuntimeHandle};
pub use snapshot::Snapshot;
