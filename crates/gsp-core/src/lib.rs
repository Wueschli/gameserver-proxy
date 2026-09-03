//! Core data plane for the game server proxy: the config snapshot, backend
//! pools with health state, the TCP listener accept loop, the byte pump, and
//! the active health checker.
//!
//! Roadmap status (`docs/08-roadmap.md`): phases 1–2 — TCP and UDP forwarding,
//! round-robin / least-connections balancing, per-backend session caps,
//! active `tcp_connect` / `udp_probe` health checks with passive failure
//! feedback, per-worker UDP session tables with `src_ip` affinity, and hot
//! reload via an atomic snapshot swap.

pub mod drain;
pub mod health;
pub mod listener;
pub mod listener_udp;
pub mod listeners;
pub mod metrics_defs;
pub mod net;
pub mod overlay;
pub mod pool;
pub mod proxy;
pub mod resolver;
pub mod route_hint;
pub mod runtime;
pub mod snapshot;
pub mod sniff;
mod util;

pub use drain::{ConnGuard, ConnTracker, DEFAULT_SHUTDOWN_GRACE};
pub use listeners::ListenerManager;
pub use overlay::BackendOverlay;
pub use resolver::{
    CachedResolver, Resolution, ResolveError, ResolveRequest, Resolver, Resolvers, Routed,
};
pub use route_hint::RouteHints;
pub use runtime::{Runtime, RuntimeHandle};
pub use snapshot::Snapshot;
