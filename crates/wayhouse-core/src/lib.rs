//! Core data plane for the game server proxy: the config snapshot, backend
//! pools with health state, the TCP listener accept loop, the byte pump, and
//! the active health checker.
//!
//! Roadmap status (`docs/08-roadmap.md`): phases 1–2 — TCP and UDP forwarding,
//! round-robin / least-connections balancing, per-backend session caps,
//! active `tcp_connect` / `udp_probe` health checks with passive failure
//! feedback, per-worker UDP session tables, and hot
//! reload via an atomic snapshot swap.

pub mod discovery;
pub mod drain;
pub mod error;
pub mod geo;
#[cfg(feature = "gossip")]
pub mod gossip;
#[cfg(not(feature = "gossip"))]
#[path = "gossip_disabled.rs"]
pub mod gossip;
pub mod health;
pub mod limits;
pub mod listener;
pub mod listener_udp;
pub mod listeners;
pub mod metrics_defs;
pub mod net;
pub mod overlay;
pub mod pool;
pub mod proxy;
pub mod proxy_protocol;
pub mod ratelimit;
pub mod resolver;
pub mod route_hint;
pub mod runtime;
pub mod snapshot;
pub mod sniff;
pub mod sources;
pub mod src_conns;
mod util;

pub use discovery::{refresh_loop, BackendSource, Discovery};
pub use drain::{ConnGuard, ConnTracker, Proto, SessionInfo, SessionMeta, DEFAULT_SHUTDOWN_GRACE};
pub use error::{ListenerError, ProxyError, SourceError};
pub use geo::GeoDb;
pub use limits::{GlobalLimits, LimitGuard};
pub use listeners::{BindError, ListenerManager, Reconciled};
pub use overlay::BackendOverlay;
pub use ratelimit::RateLimiter;
pub use resolver::{
    CachedResolver, Resolution, ResolveError, ResolveRequest, Resolver, Resolvers, Routed,
};
pub use route_hint::RouteHints;
pub use runtime::{Runtime, RuntimeHandle};
pub use snapshot::Snapshot;
pub use sources::{SourceFactory, SourceManager};
pub use src_conns::{SourceGuard, SourceLimiter};
