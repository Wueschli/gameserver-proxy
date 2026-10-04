//! Validated types: parsed, resolved, ready for the runtime to consume.

use std::net::SocketAddr;
use std::ops::RangeInclusive;
use std::time::Duration;

use serde::Deserialize;

use crate::cidr::{Acl, Cidr, GeoAcl};
use crate::matcher::{MatchContext, Matcher, PEEK_MAX};
use crate::schema::AdminTls;

// ---------------------------------------------------------------------------
// Shared enums.
// ---------------------------------------------------------------------------

/// Backend selection strategy within a pool.
#[derive(Debug, Deserialize, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Balancer {
    #[default]
    RoundRobin,
    LeastConn,
    /// Rendezvous (HRW) hash of a per-client key over the healthy backends: the
    /// same key sticks to the same backend without a session table, and only
    /// that backend's share moves when the set changes. Key selected by the
    /// pool's `hash_on` (`src_ip` default).
    ConsistentHash,
    /// Round-robin biased by a per-backend `weight` (from the pool's `weights`
    /// map; any backend not listed there weighs `1`). A backend weighing `3`
    /// receives three times the new sessions of a backend weighing `1`.
    Weighted,
}

/// Listener transport.
#[derive(Debug, Deserialize, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    #[default]
    Tcp,
    Udp,
}

/// What a UDP session's client key hashes on for backend stickiness.
#[derive(Debug, Deserialize, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HashOn {
    #[default]
    SrcIp,
    SrcIpPort,
}

/// Whether to prepend a [PROXY protocol] header to the upstream connection so
/// the backend learns the real client address. `none` (default) sends nothing;
/// `v1` sends the human-readable text header; `v2` sends the binary header.
/// Only the first bytes toward the backend carry it (TCP: before any payload).
///
/// [PROXY protocol]: https://www.haproxy.org/download/1.8/doc/proxy-protocol.txt
#[derive(Debug, Deserialize, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProxyProtocol {
    #[default]
    None,
    /// TCP text header. TCP listeners only.
    V1,
    /// TCP binary header. TCP listeners only.
    V2,
    /// Binary header prepended to the **first datagram** of a UDP session.
    /// UDP listeners only.
    #[serde(rename = "v2-udp")]
    V2Udp,
}

impl ProxyProtocol {
    /// Metrics label.
    pub fn label(self) -> &'static str {
        match self {
            ProxyProtocol::None => "none",
            ProxyProtocol::V1 => "v1",
            ProxyProtocol::V2 => "v2",
            ProxyProtocol::V2Udp => "v2-udp",
        }
    }
}

/// Health probe variant. `tcp_connect` just opens a TCP connection; `udp_probe`
/// sends `send` and expects a reply datagram (optionally prefix-matched).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HealthCheckKind {
    TcpConnect,
    UdpProbe {
        send: Vec<u8>,
        expect_prefix: Vec<u8>,
    },
}

/// One token bucket's parameters. `rate` permits/second sustained, `burst`
/// capacity. `gsp-core` owns the live bucket state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenBucket {
    pub rate: u32,
    pub burst: u32,
}

/// Per-listener rate limit (phase 7). At least one of `per_ip` / `per_net` is
/// `Some` when this is present. Keyed on the client source IP and its /24 (v4)
/// or /64 (v6) network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimit {
    pub per_ip: Option<TokenBucket>,
    pub per_net: Option<TokenBucket>,
}

/// Per-listener cap on *concurrent* connections / UDP sessions from one source
/// (phase 7), by client IP and/or its /24 (v4) / /64 (v6). At least one field
/// is `Some` when this is present. The live counters live in `gsp-core`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PerSourceLimit {
    pub max_per_ip: Option<usize>,
    pub max_per_net: Option<usize>,
}

/// What a matched route does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Route to this pool (normal load balancing).
    Pool(String),
    /// Ask this named [`ResolverConfig`] which pool / target to use.
    Resolver(String),
}

/// One rule in a listener's ordered route list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub matcher: Matcher,
    pub action: Action,
}

/// What to do when an external resolver call fails or times out.
#[derive(Debug, Deserialize, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OnError {
    /// Drop the connection / datagram.
    #[default]
    Reject,
    /// Treat the resolver route as "did not match" and continue the route list.
    FallbackRoute,
    /// Serve the last successful (now-expired) cached answer if there is one,
    /// else `reject`. (Needs the resolver cache — phase 4 slice 2.)
    StaleOk,
}

/// Transport for an external resolver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolverKind {
    Http,
    Grpc,
}

/// One part of a resolver cache key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheKeyPart {
    SrcIp,
    SrcIpPort,
    Sni,
    RoutingKey,
    /// A slice of the first bytes (`first_bytes:a:b`, `a..b`).
    FirstBytes(RangeInclusive<usize>),
}

/// Resolver result cache (`resolvers[].cache`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheConfig {
    pub key: Vec<CacheKeyPart>,
    pub positive_ttl: Duration,
    pub negative_ttl: Duration,
    pub max_entries: usize,
}

/// An external routing resolver (`resolvers:` entry). `PartialEq` so the reload
/// task can tell when `resolvers:` actually changed and rebuild the clients only
/// then (a plain reload / discovery tick must not drop the LRU caches).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolverConfig {
    pub name: String,
    pub kind: ResolverKind,
    pub endpoint: String,
    pub timeout: Duration,
    pub on_error: OnError,
    /// `Some` if `cache:` is set with a non-empty `key`.
    pub cache: Option<CacheConfig>,
    /// PROXY protocol header for a `target` result (pool-less connect). `None`
    /// for a resolver-chosen pool, which carries its own `proxy_protocol`.
    pub proxy_protocol: ProxyProtocol,
    /// Connect / idle timeout for a `target` result (pool-less connect — no pool
    /// to read `connect_timeout_ms` / `idle_timeout_sec` from). Default 300 ms /
    /// 90 s.
    pub target_connect_timeout: Duration,
    pub target_idle_timeout: Duration,
}

// ---------------------------------------------------------------------------
// Validated types: parsed, resolved, ready for the runtime to consume.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Config {
    /// `0` means "one worker per CPU core".
    pub workers: usize,
    /// How long `shutdown` waits for in-flight connections to finish.
    pub shutdown_grace: Duration,
    pub admin_listen: SocketAddr,
    /// Bearer token every admin API request (except `GET /healthz`) must
    /// present; `None` leaves the API open.
    pub admin_auth_token: Option<String>,
    /// `settings.admin.tls`: serve the admin API over HTTPS. Startup-only.
    pub admin_tls: Option<AdminTls>,
    pub pools: Vec<PoolConfig>,
    pub resolvers: Vec<ResolverConfig>,
    pub listeners: Vec<ListenerConfig>,
    /// Process-wide caps. Startup-only — a reload does not change them.
    pub limits: GlobalLimits,
    /// Path to the MaxMind Country DB, if any listener uses a `geo` filter.
    /// Startup-only.
    pub geo_db: Option<String>,
    /// Sniffer plugin loader settings (phase 9). `None` ⇒ no plugins load.
    pub sniffers: Option<SniffersConfig>,
    /// Tier-2 regional health fabric identity (phase 13). `None` ⇒ gossip
    /// fully disabled, today's local-only health behaviour.
    pub failure_domain: Option<String>,
    pub gossip: Option<GossipConfig>,
    /// Self-reported fleet organization path (e.g. `"eu/frankfurt/cluster-a"`).
    /// `None` ⇒ this instance shows up ungrouped in the admin GUI's fleet
    /// tree. Never consulted by routing/forwarding.
    pub group: Option<String>,
}

/// Resolved `settings.gossip` (phase 13, docs/10 "Tier 2"). The mesh itself
/// (`foca`, the UDP socket, the HMAC auth) lives in `gsp-core::gossip` — this
/// is just the validated config it reads.
#[derive(Debug, Clone)]
pub struct GossipConfig {
    pub bind: SocketAddr,
    pub seeds: Vec<SocketAddr>,
    pub quorum_fraction: f64,
    pub psk: String,
}

/// Resolved `settings.sniffers` (phase 9). The loader itself (`wasmtime`, the
/// ABI, the plugin crates) lives in the `gsp` binary — this is just the
/// validated config it reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SniffersConfig {
    pub dir: String,
    pub call_timeout: Duration,
    pub max_memory_bytes: usize,
    /// Supply-chain pins. Empty ⇒ any `*.wasm` in `dir` loads unchecked.
    pub modules: Vec<SnifferModulePin>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnifferModulePin {
    pub name: String,
    pub sha256: String,
    /// Opaque per-plugin config string (see `RawSnifferModule::config`). `None`
    /// ⇒ the module is handed an empty config on each `sniff` call.
    pub config: Option<String>,
}

/// Process-wide resource caps (phase 7). `None` fields = uncapped. The live
/// counters / token bucket live in `gsp-core`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GlobalLimits {
    pub max_connections: Option<usize>,
    pub max_udp_sessions: Option<usize>,
    pub max_new_sessions_per_sec: Option<u32>,
}

impl GlobalLimits {
    pub fn is_empty(&self) -> bool {
        self.max_connections.is_none()
            && self.max_udp_sessions.is_none()
            && self.max_new_sessions_per_sec.is_none()
    }
}

#[derive(Debug, Clone)]
pub struct HealthCheck {
    pub kind: HealthCheckKind,
    pub interval: Duration,
    pub timeout: Duration,
    /// Consecutive successes needed to mark an unhealthy backend healthy.
    pub rise: u32,
    /// Consecutive failures needed to mark a healthy backend unhealthy.
    pub fall: u32,
}

/// A resolved dynamic backend discovery source attached to a pool (phase 8).
/// `static` sources are folded into `PoolConfig::targets` and never appear here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceConfig {
    /// The `backend_sources[].name` (for logs / the `source` metric label).
    pub name: String,
    pub kind: SourceKind,
    /// Control-plane refresh cadence.
    pub refresh_interval: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceKind {
    /// Resolve an SRV record; the port comes from each record.
    DnsSrv { record: String },
    /// Consul health API: passing instances of a service.
    Consul {
        service: String,
        addr: String,
        tag: Option<String>,
    },
    /// Kubernetes EndpointSlices of a service (polled and watched).
    Kubernetes {
        namespace: String,
        service: String,
        port_name: Option<String>,
        api: String,
    },
    /// Phase 14: backend addresses registered by a `gsp-agent`-managed origin
    /// behind a WireGuard tunnel, resolved via the controller's backend-peers
    /// registry (slice 2). `pubkey` pins the origin's expected WireGuard key.
    Tunnel { pubkey: String },
}

#[derive(Debug, Clone)]
pub struct PoolConfig {
    pub name: String,
    /// Static targets, or the discovery *seed* (empty until first refresh) when
    /// `source` is set.
    pub targets: Vec<SocketAddr>,
    /// `Some` ⇒ backends are discovered by a control-plane refresh task; the
    /// discovered set replaces `targets` in every snapshot rebuild.
    pub source: Option<SourceConfig>,
    pub balancer: Balancer,
    /// `Some` iff `balancer == ConsistentHash`: the hash key selector.
    pub hash_on: Option<HashOn>,
    /// `balancer == Weighted` only (empty otherwise): backend address → weight
    /// (>= 1). A backend not present weighs `1`.
    pub weights: std::collections::HashMap<SocketAddr, u32>,
    pub connect_timeout: Duration,
    pub idle_timeout: Duration,
    pub health_check: HealthCheck,
    /// Max concurrent sessions per backend, if capped.
    pub max_sessions: Option<usize>,
    /// PROXY protocol header to prepend to the upstream connection.
    pub proxy_protocol: ProxyProtocol,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListenerConfig {
    pub name: String,
    /// Primary (lowest-port) bind address. For a plain `"host:port"` bind
    /// this is the whole listener; for a `"host:lo-hi"` port-range bind
    /// (F1.4) it's the first port, with the rest in `extra_binds` — one real
    /// socket per port, all sharing this listener's routes/filters. A route's
    /// `port` matcher still sees the specific port a connection/datagram
    /// actually arrived on, via `ctx.local`, not this field.
    pub bind: SocketAddr,
    /// The rest of a `"host:lo-hi"` bind range, if any (empty for a plain
    /// single-port bind).
    pub extra_binds: Vec<SocketAddr>,
    pub protocol: Protocol,
    /// Priority-ordered; the first matching route's pool wins. A listener with a
    /// bare `pool:` is normalised to one `always` route here.
    pub routes: Vec<Route>,
    /// `Some` on UDP listeners (stickiness key); `None` on TCP.
    pub affinity: Option<HashOn>,
    /// `Some` (UDP only) ⇒ prefix mode: wildcard-bind + `IP_PKTINFO` so one
    /// socket serves the whole routed prefix; datagrams to a destination outside
    /// it are dropped.
    pub prefix: Option<Cidr>,
    /// TCP only: bind with `IP_FREEBIND` / `IPV6_FREEBIND`.
    pub freebind: bool,
    /// TCP only (Linux): transparent mode — `IP_TRANSPARENT` on the listen
    /// socket and a client-address-bound `IP_TRANSPARENT` upstream socket per
    /// connection. Needs `CAP_NET_ADMIN`.
    pub transparent: bool,
    /// The distinct sniffer plugins this listener's routes use, in order of
    /// first appearance (empty if no `sniffer` route). `gsp-core` runs them in
    /// that order once per connection / first datagram before routing; the
    /// first one that recognises the bytes wins and its hint is the only one
    /// routing sees.
    pub sniffers: Vec<String>,
    /// Check the `POST /route-hint` push-resolver table before the route list.
    pub route_hint: bool,
    /// UDP only: gate new sessions on positive first-datagram recognition.
    pub first_packet_gate: bool,
    /// Source-IP filter chain, checked before routing. Empty ⇒ admit everyone.
    pub acl: Acl,
    /// GeoIP country filter, checked after `acl`. `None` ⇒ no geo check.
    pub geo: Option<GeoAcl>,
    /// Concurrent per-source connection / session cap. `None` ⇒ no cap.
    pub per_source: Option<PerSourceLimit>,
    /// Token-bucket rate limit on new connections / UDP sessions. `None` ⇒ no
    /// limit. Checked after the ACL.
    pub rate_limit: Option<RateLimit>,
}

impl ListenerConfig {
    /// Every real socket address this listener binds — `bind` plus
    /// `extra_binds` (a plain single-port listener yields just `bind`).
    pub fn binds(&self) -> impl Iterator<Item = SocketAddr> + '_ {
        std::iter::once(self.bind).chain(self.extra_binds.iter().copied())
    }

    /// The routes whose matcher fires for `ctx`, in priority order. The runtime
    /// walks this: a `Pool` action ends the walk; a `Resolver` action may
    /// continue it (`on_error: fallback_route`).
    pub fn matching_routes<'a>(
        &'a self,
        ctx: &'a MatchContext<'a>,
    ) -> impl Iterator<Item = &'a Route> {
        self.routes.iter().filter(move |r| r.matcher.matches(ctx))
    }

    /// Convenience for the common case / tests: the first matching route's pool,
    /// or `None` if nothing matches or the first match is a `Resolver` action.
    pub fn route_for(&self, ctx: &MatchContext) -> Option<&str> {
        match &self.routes.iter().find(|r| r.matcher.matches(ctx))?.action {
            Action::Pool(p) => Some(p.as_str()),
            Action::Resolver(_) => None,
        }
    }

    /// How many leading bytes to `MSG_PEEK` before routing (0 = no byte
    /// matcher / resolver, skip the peek entirely).
    pub fn peek_len(&self) -> usize {
        let from_matchers = self.routes.iter().map(|r| r.matcher.peek_len()).max();
        let needs_resolver = self
            .routes
            .iter()
            .any(|r| matches!(r.action, Action::Resolver(_)));
        from_matchers
            .unwrap_or(0)
            .max(if needs_resolver { PEEK_MAX } else { 0 })
    }

    /// UDP first-packet gate (phase 7): is the first datagram positively
    /// recognised — the sniffer produced a non-`reject` hint, or a `first_bytes`
    /// route on this listener matches it? Only consulted when
    /// `first_packet_gate` is set.
    pub fn first_packet_recognised(&self, ctx: &MatchContext) -> bool {
        if ctx.sniff.is_some_and(|(_, h)| !h.reject) {
            return true;
        }
        self.routes
            .iter()
            .any(|r| matches!(r.matcher, Matcher::FirstBytes { .. }) && r.matcher.matches(ctx))
    }
}
