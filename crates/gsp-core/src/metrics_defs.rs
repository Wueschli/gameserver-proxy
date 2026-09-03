//! Canonical metric names. Kept in one place so names stay stable.
//! See `docs/06-operations-observability.md` for the full metric catalogue.

/// Counter. Labels: `listener`, `result` (`accepted` | `no_route` | ...).
pub const LISTENER_CONNECTIONS: &str = "gsp_listener_connections_total";

/// Gauge. Labels: `listener`.
pub const ACTIVE_CONNECTIONS: &str = "gsp_active_connections";

/// Counter. Labels: `listener`, `dir` (`c2s` | `s2c`).
pub const BYTES: &str = "gsp_bytes_total";

/// Histogram (seconds). Labels: `listener`.
pub const CONNECTION_DURATION: &str = "gsp_connection_duration_seconds";

/// Counter. Labels: `backend`, `kind` (`timeout` | `refused` | `unreachable`).
pub const BACKEND_CONNECT_ERRORS: &str = "gsp_backend_connect_errors_total";

/// Gauge. Labels: `pool`, `state` (`healthy` | `unhealthy`).
pub const POOL_BACKENDS: &str = "gsp_pool_backends";

/// Gauge. Labels: `pool`, `backend`.
pub const BACKEND_ACTIVE_SESSIONS: &str = "gsp_backend_active_sessions";

/// Counter. Labels: `pool`, `backend`, `result` (`ok` | `fail`).
pub const HEALTHCHECK: &str = "gsp_healthcheck_total";

/// Counter. Labels: `pool`, `strategy`, `result` (`ok` | `no_backend` | `at_capacity`).
pub const LB_SELECTIONS: &str = "gsp_lb_selections_total";

/// Counter. Labels: `result` (`ok` | `failed`).
pub const CONFIG_RELOAD: &str = "gsp_config_reload_total";

/// Gauge: unix timestamp of the last successfully applied config.
pub const CONFIG_VERSION: &str = "gsp_config_version";
