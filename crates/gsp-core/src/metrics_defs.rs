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
