//! Canonical metric names. Kept in one place so names stay stable.
//! See `docs/06-operations-observability.md` for the full metric catalogue.

/// Counter. Labels: `listener`, `result` (`accepted` | `no_route` |
/// `sniffer_reject` | ...). `sniffer_reject` = a `sniffer` plugin returned a
/// `reject` hint for the connection's first bytes; it is dropped before routing.
pub const LISTENER_CONNECTIONS: &str = "gsp_listener_connections_total";

/// Gauge. Labels: `listener`.
pub const ACTIVE_CONNECTIONS: &str = "gsp_active_connections";

/// Gauge. Labels: `listener`. Live UDP sessions across the worker-local tables.
pub const ACTIVE_UDP_SESSIONS: &str = "gsp_active_udp_sessions";

/// Counter. Labels: `listener`, `dir` (`c2s` | `s2c`). UDP datagrams forwarded.
pub const PACKETS: &str = "gsp_packets_total";

/// Counter. Labels: `listener`, `reason`
/// (`no_route` | `no_backend` | `upstream_bind` | `upstream_send` |
/// `outside_prefix` | `draining` | `reply_bind` | `first_packet_gate` |
/// `sniffer_reject`). `sniffer_reject` = a `sniffer` plugin returned a `reject`
/// hint for the first datagram; no session opens and no reply is sent.
pub const DATAGRAMS_DROPPED: &str = "gsp_datagrams_dropped_total";

/// Counter. Labels: `listener`, `dir` (`c2s` | `s2c`).
pub const BYTES: &str = "gsp_bytes_total";

/// Histogram (seconds). Labels: `listener`.
pub const CONNECTION_DURATION: &str = "gsp_connection_duration_seconds";

/// Counter. Labels: `backend`, `kind` (`timeout` | `refused` | `unreachable`).
pub const BACKEND_CONNECT_ERRORS: &str = "gsp_backend_connect_errors_total";

/// Gauge. Labels: `pool`, `state` (`healthy` | `unhealthy` | `draining` | `disabled`).
pub const POOL_BACKENDS: &str = "gsp_pool_backends";

/// Gauge. Labels: `pool`, `backend`.
pub const BACKEND_ACTIVE_SESSIONS: &str = "gsp_backend_active_sessions";

/// Counter. Labels: `pool`, `backend`, `result` (`ok` | `fail`).
pub const HEALTHCHECK: &str = "gsp_healthcheck_total";

/// Counter. Labels: `pool`, `strategy`, `result` (`ok` | `no_backend` | `at_capacity`).
pub const LB_SELECTIONS: &str = "gsp_lb_selections_total";

/// Counter. Labels: `listener`. A `POST /route-hint` entry decided routing for a
/// connection / UDP session (bypassing the route list).
pub const ROUTE_HINTS_APPLIED: &str = "gsp_route_hints_applied_total";

/// Counter. Labels: `resolver`, `result` (`ok` | `empty` | `timeout` | `error`).
pub const RESOLVER_REQUESTS: &str = "gsp_resolver_requests_total";

/// Counter. Labels: `resolver`, `result`
/// (`hit` | `hit_negative` | `miss` | `stale` | `uncacheable`).
pub const RESOLVER_CACHE: &str = "gsp_resolver_cache_total";

/// Counter. Labels: `pool`, `version` (`v1` | `v2` | `v2-udp`). A PROXY protocol
/// header was prepended to an upstream connection (TCP) or the first datagram of
/// a session (UDP `v2-udp`).
pub const PROXY_PROTOCOL_HEADERS: &str = "gsp_proxy_protocol_headers_total";

/// Counter. Labels: `listener`, `filter` (`acl` | `geo` | `rate_ip` |
/// `rate_net` | `src_conn_ip` | `src_conn_net` | `max_conn` | `max_udp` |
/// `max_new_rate`). A connection / new UDP session was dropped by the
/// pre-routing filter chain: `acl` = source IP allow/deny; `geo` = GeoIP country
/// allow/deny (or the DB failed to load and the listener fails closed);
/// `rate_ip` / `rate_net` = the per-listener per-IP / per-/24 (per-/64) token
/// bucket was empty; `src_conn_ip` / `src_conn_net` = the per-listener
/// concurrent per-source connection / session cap was reached; `max_conn` /
/// `max_udp` / `max_new_rate` = a process-wide `settings.limits` cap was hit.
pub const FILTER_BLOCKED: &str = "gsp_filter_blocked_total";

/// Counter. Labels: `pool`, `kind` (`dns_srv` | `consul` | `kubernetes`),
/// `result` (`ok` | `empty` | `error`). One increment per backend-discovery
/// refresh attempt. `empty` / `error` keep the last-known-good backend set.
pub const DISCOVERY_REFRESH: &str = "gsp_discovery_refresh_total";

/// Gauge. Labels: `pool`. Backend addresses returned by the pool's discovery
/// source at its last successful refresh.
pub const DISCOVERY_BACKENDS: &str = "gsp_discovery_backends";

/// Counter. Labels: `result` (`ok` | `failed`).
pub const CONFIG_RELOAD: &str = "gsp_config_reload_total";

/// Gauge: unix timestamp of the last successfully applied config.
pub const CONFIG_VERSION: &str = "gsp_config_version";

/// Counter. Labels: `name` (the sniffer plugin), `result` (`ok` |
/// `unrecognised` | `timeout` | `trap` | `bad_output`). One increment per
/// `Sniffer::sniff` call through the phase 9 WASM loader (`ok` = recognised,
/// `unrecognised` = a clean "no match", `timeout` = the epoch-interruption
/// deadline fired, `trap` = any other WASM trap / instantiation error,
/// `bad_output` = the plugin returned a result the host couldn't decode).
pub const SNIFFER_CALLS: &str = "gsp_sniffer_calls_total";

/// Histogram (seconds). Labels: `name`. Wall-clock time of one `sniff` call
/// through the WASM loader, instantiation included.
pub const SNIFFER_CALL_SECONDS: &str = "gsp_sniffer_call_seconds";

/// Gauge. Labels: `version`, `commit`. Always `1`; set once at startup so the
/// labels themselves carry the build identity for correlating a metric shift
/// with a deploy.
pub const BUILD_INFO: &str = "gsp_build_info";

/// Gauge, no labels. The process's own open file descriptor count, sampled on
/// the health-check sweep interval (`/proc/self/fd` on Linux; absent
/// elsewhere).
pub const FD_OPEN: &str = "gsp_fd_open";

/// Gauge, no labels. The process's `RLIMIT_NOFILE` soft limit, sampled once at
/// startup (it does not change at runtime).
pub const FD_LIMIT: &str = "gsp_fd_limit";

/// Gauge, no labels. Current SWIM member count in this instance's gossip mesh
/// (phase 13, `gsp-core::gossip`). Present only when `settings.gossip` is set.
pub const GOSSIP_MEMBERS: &str = "gsp_gossip_members";

/// Counter. Labels: `direction` (`sent` | `received`). Gossip datagrams that
/// passed HMAC verification (phase 13).
pub const GOSSIP_MESSAGES_TOTAL: &str = "gsp_gossip_messages_total";

/// Counter, no labels. Gossip datagrams dropped for a missing or invalid HMAC
/// tag (phase 13) — never trusted, never a panic.
pub const GOSSIP_AUTH_REJECTED_TOTAL: &str = "gsp_gossip_auth_rejected_total";

/// Counter, no labels. Authentic gossip datagrams dropped because their sender
/// timestamp is more than 30 s from this node's clock: a replay, or an
/// instance with a badly skewed clock.
pub const GOSSIP_STALE_REJECTED_TOTAL: &str = "gsp_gossip_stale_rejected_total";

/// Gauge (0/1). Labels: `pool`, `backend`. Whether the Tier-2 domain quorum
/// is currently overriding this backend to down (phase 13) — independent of
/// (and never able to clear) the backend's own local `healthy` flag.
pub const BACKEND_DOMAIN_DOWN: &str = "gsp_backend_domain_down";
