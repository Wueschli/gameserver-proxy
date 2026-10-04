//! Raw types: a 1:1 mapping of the YAML document.

use serde::Deserialize;

use crate::resolved::{Balancer, HashOn, OnError, Protocol, ProxyProtocol};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawConfig {
    #[serde(default)]
    pub(crate) settings: RawSettings,
    #[serde(default)]
    pub(crate) backend_sources: Vec<RawBackendSource>,
    #[serde(default)]
    pub(crate) pools: Vec<RawPool>,
    #[serde(default)]
    pub(crate) resolvers: Vec<RawResolver>,
    #[serde(default)]
    pub(crate) listeners: Vec<RawListener>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawSettings {
    /// Accept-loop tasks per listener. `0` means "one per CPU core".
    #[serde(default)]
    pub(crate) workers: usize,
    /// Grace period for in-flight connections on shutdown, in seconds.
    #[serde(default = "default_shutdown_grace_sec")]
    pub(crate) shutdown_grace_sec: u64,
    #[serde(default)]
    pub(crate) admin: RawAdmin,
    /// Process-wide caps (phase 7). Startup-only (like `workers`).
    #[serde(default)]
    pub(crate) limits: RawLimits,
    /// Path to a MaxMind Country `.mmdb`, required when any listener has a `geo`
    /// filter. Startup-only.
    #[serde(default)]
    pub(crate) geo_db: Option<String>,
    /// Sniffer plugin loader (phase 9). Absent ⇒ no plugins load; a `sniffer:`
    /// route never matches. Startup-only for `dir` itself (rescanned on
    /// reload once the loader lands — phase 9 slice 4).
    #[serde(default)]
    pub(crate) sniffers: Option<RawSniffers>,
    /// Tier-2 regional health fabric (phase 13, docs/10 "Tier 2"). This
    /// instance's reachability-equivalence class. Must be set together with
    /// `gossip`, or not at all. Startup-only.
    #[serde(default)]
    pub(crate) failure_domain: Option<String>,
    /// Tier-2 gossip mesh membership (phase 13). Must be set together with
    /// `failure_domain`, or not at all. Startup-only.
    #[serde(default)]
    pub(crate) gossip: Option<RawGossip>,
    /// Self-reported fleet organization path (e.g. `"eu/frankfurt/cluster-a"`),
    /// pushed to gsp-aggregator alongside this instance's `IngestPayload` so
    /// the admin GUI can render a grouped/tree view. Purely a fleet-display
    /// label — never consulted by routing/forwarding. `/`-separated,
    /// non-empty segments, no leading/trailing `/`.
    #[serde(default)]
    pub(crate) group: Option<String>,
}

impl Default for RawSettings {
    fn default() -> Self {
        Self {
            workers: 0,
            shutdown_grace_sec: default_shutdown_grace_sec(),
            admin: RawAdmin::default(),
            limits: RawLimits::default(),
            geo_db: None,
            sniffers: None,
            failure_domain: None,
            gossip: None,
            group: None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawGossip {
    /// UDP address this instance's gossip mesh listens/sends on.
    pub(crate) bind: String,
    /// Known peers to bootstrap membership from (any subset of the domain's
    /// live members is enough — foca's own SWIM traffic discovers the rest).
    #[serde(default)]
    pub(crate) seeds: Vec<String>,
    /// Fraction of the domain's known members that must report a backend
    /// down for that verdict to override this instance's own "up" reading.
    /// Must be > 0.5 and <= 1.0 (a same-or-under-half quorum could contradict
    /// itself between two overlapping majorities).
    #[serde(default = "default_gossip_quorum_fraction")]
    pub(crate) quorum_fraction: f64,
    /// Pre-shared key: every gossip datagram carries an HMAC-SHA256 tag
    /// computed with it. A wrong or missing tag is dropped silently.
    pub(crate) psk: String,
}

pub(crate) fn default_gossip_quorum_fraction() -> f64 {
    0.66
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawSniffers {
    /// Directory scanned for `*.wasm` plugin modules.
    pub(crate) dir: String,
    /// Per-call wall-clock budget (epoch interruption traps a plugin that
    /// overruns this).
    #[serde(default = "default_sniffer_call_timeout_ms")]
    pub(crate) call_timeout_ms: u64,
    /// Per-call memory ceiling for a plugin instance.
    #[serde(default = "default_sniffer_max_memory_bytes")]
    pub(crate) max_memory_bytes: usize,
    /// Optional supply-chain pin: a module whose file name isn't listed here,
    /// or whose SHA-256 doesn't match, is refused at load time.
    #[serde(default)]
    pub(crate) modules: Vec<RawSnifferModule>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawSnifferModule {
    pub(crate) name: String,
    pub(crate) sha256: String,
    /// Opaque per-plugin configuration string, handed to the module on every
    /// `sniff` call (the plugin parses it however it likes — e.g. a pattern for
    /// `regex-firstbytes`). Omitted ⇒ the plugin gets an empty config.
    #[serde(default)]
    pub(crate) config: Option<String>,
}

pub(crate) fn default_sniffer_call_timeout_ms() -> u64 {
    20
}

pub(crate) fn default_sniffer_max_memory_bytes() -> usize {
    16 * 1024 * 1024
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawLimits {
    /// Max live proxied TCP connections across all listeners. Absent ⇒ no cap.
    #[serde(default)]
    pub(crate) max_connections: Option<usize>,
    /// Max live UDP sessions across all listeners. Absent ⇒ no cap.
    #[serde(default)]
    pub(crate) max_udp_sessions: Option<usize>,
    /// Max new connections + UDP sessions accepted per second (token bucket,
    /// burst = the rate). Absent ⇒ no cap.
    #[serde(default)]
    pub(crate) max_new_sessions_per_sec: Option<u32>,
}

pub(crate) fn default_shutdown_grace_sec() -> u64 {
    30
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawAdmin {
    #[serde(default = "default_admin_listen")]
    pub(crate) listen: String,
    /// Bearer token every admin API request (except `GET /healthz`) must
    /// present. `None` leaves the API open — network-boundary-only auth,
    /// same posture as always. Needed once anything calls in from outside
    /// that boundary — today, `gsp-aggregator`'s intent-verb fan-out
    /// (phase 10+11 slice 10, `docs/10` "The aggregator").
    #[serde(default)]
    pub(crate) auth_token: Option<String>,
    /// Serve the admin API over HTTPS with this certificate pair.
    #[serde(default)]
    pub(crate) tls: Option<AdminTls>,
}

impl Default for RawAdmin {
    fn default() -> Self {
        Self {
            listen: default_admin_listen(),
            auth_token: None,
            tls: None,
        }
    }
}

/// `settings.admin.tls`: the admin API serves HTTPS with this pair (PEM chain,
/// leaf first; PEM private key). Both or neither, by type. Startup-only like
/// `listen`; the files themselves are re-read when they change.
///
/// The optional `max_pending*` / `new_per_source_*` settings bound the TLS
/// handshakes in flight (same as `gsp-controller`'s `--tls-max-pending*` /
/// `--tls-new-per-source-*` flags); unset ones keep the defaults.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminTls {
    pub cert: String,
    pub key: String,
    /// Handshakes in flight in total (default 512, at least 1).
    #[serde(default)]
    pub max_pending: Option<usize>,
    /// Handshakes in flight per source: an IPv4 address or an IPv6 /64 (default
    /// 16, at least 1).
    #[serde(default)]
    pub max_pending_per_source: Option<usize>,
    /// New connections per second a source may open on average (default 20;
    /// `0` = no rate limit).
    #[serde(default)]
    pub new_per_source_per_sec: Option<f64>,
    /// Burst a source may open at once (default 64, at least 1).
    #[serde(default)]
    pub new_per_source_burst: Option<u32>,
}

pub(crate) fn default_admin_listen() -> String {
    "127.0.0.1:9900".to_string()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawPool {
    pub(crate) name: String,
    /// Static backend list. Mutually exclusive with `source`.
    #[serde(default)]
    pub(crate) targets: Vec<String>,
    /// Name of a `backend_sources[]` entry that discovers this pool's backends.
    /// Mutually exclusive with `targets`.
    #[serde(default)]
    pub(crate) source: Option<String>,
    #[serde(default)]
    pub(crate) balancer: Balancer,
    /// `consistent_hash` only: which part of the client address is the hash key.
    #[serde(default)]
    pub(crate) hash_on: Option<HashOn>,
    /// `weighted` only: `"ip:port"` → weight (>= 1). A backend (static or
    /// discovered) not listed here weighs `1`.
    #[serde(default)]
    pub(crate) weights: std::collections::HashMap<String, u32>,
    #[serde(default = "default_connect_timeout_ms")]
    pub(crate) connect_timeout_ms: u64,
    #[serde(default = "default_idle_timeout_sec")]
    pub(crate) idle_timeout_sec: u64,
    #[serde(default)]
    pub(crate) health_check: RawHealthCheck,
    #[serde(default)]
    pub(crate) per_backend: RawPerBackend,
    /// Prepend a PROXY protocol header to the upstream connection.
    #[serde(default)]
    pub(crate) proxy_protocol: ProxyProtocol,
}

pub(crate) fn default_connect_timeout_ms() -> u64 {
    300
}

pub(crate) fn default_idle_timeout_sec() -> u64 {
    90
}

/// A named backend discovery source (phase 8). Level-triggered: an adapter
/// returns the *current* address set for the pool; the runtime diffs it against
/// the live set. `static` is resolved to the pool's `targets` at validation
/// time; the other kinds get a control-plane refresh task in `gsp-core`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawBackendSource {
    pub(crate) name: String,
    #[serde(rename = "type")]
    pub(crate) kind: String,
    /// How often the refresh task re-queries the source (dynamic kinds only).
    #[serde(default = "default_source_refresh_sec")]
    pub(crate) refresh_interval_sec: u64,
    /// `static` only: the fixed address list.
    #[serde(default)]
    pub(crate) targets: Vec<String>,
    /// `dns_srv` only: the SRV record to resolve (port comes from the record).
    #[serde(default)]
    pub(crate) record: Option<String>,
    /// `consul` / `kubernetes` only: the service name.
    #[serde(default)]
    pub(crate) service: Option<String>,
    /// `consul` only: base URL of the Consul HTTP API.
    #[serde(default)]
    pub(crate) consul_addr: Option<String>,
    /// `consul` only: file holding an ACL token, sent as `X-Consul-Token`.
    /// Re-read while running, so a rotated token is picked up. A path, never the
    /// token itself, so `GET /config` and the YAML carry no secret.
    #[serde(default)]
    pub(crate) consul_token_file: Option<String>,
    /// `consul` only: restrict to service instances carrying this tag.
    #[serde(default)]
    pub(crate) tag: Option<String>,
    /// `kubernetes` only: the namespace (default `default`).
    #[serde(default)]
    pub(crate) namespace: Option<String>,
    /// `kubernetes` only: pick this named port from each EndpointSlice;
    /// absent ⇒ the first port.
    #[serde(default)]
    pub(crate) port_name: Option<String>,
    /// `kubernetes` only: API server base URL (default the in-cluster address).
    #[serde(default)]
    pub(crate) api: Option<String>,
    /// `tunnel` only: the origin's WireGuard public key (base64, 32 bytes) —
    /// identifies which `gsp-agent`-registered origin this source resolves
    /// backends from, and pins the key `gsp` expects that origin to present.
    #[serde(default)]
    pub(crate) pubkey: Option<String>,
}

pub(crate) fn default_source_refresh_sec() -> u64 {
    15
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawHealthCheck {
    #[serde(rename = "type", default = "default_hc_type")]
    pub(crate) kind: String,
    #[serde(default = "default_hc_interval_sec")]
    pub(crate) interval_sec: u64,
    #[serde(default = "default_hc_timeout_ms")]
    pub(crate) timeout_ms: u64,
    #[serde(default = "default_hc_rise")]
    pub(crate) rise: u32,
    #[serde(default = "default_hc_fall")]
    pub(crate) fall: u32,
    /// `udp_probe` only: hex-encoded payload to send to the backend.
    #[serde(default)]
    pub(crate) send_hex: Option<String>,
    /// `udp_probe` only: hex-encoded prefix the reply must start with. Empty /
    /// absent means "any reply datagram counts as healthy".
    #[serde(default)]
    pub(crate) expect_hex_prefix: Option<String>,
}

impl Default for RawHealthCheck {
    fn default() -> Self {
        Self {
            kind: default_hc_type(),
            interval_sec: default_hc_interval_sec(),
            timeout_ms: default_hc_timeout_ms(),
            rise: default_hc_rise(),
            fall: default_hc_fall(),
            send_hex: None,
            expect_hex_prefix: None,
        }
    }
}

pub(crate) fn default_hc_type() -> String {
    "tcp_connect".to_string()
}
pub(crate) fn default_hc_interval_sec() -> u64 {
    2
}
pub(crate) fn default_hc_timeout_ms() -> u64 {
    500
}
pub(crate) fn default_hc_rise() -> u32 {
    2
}
pub(crate) fn default_hc_fall() -> u32 {
    3
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawPerBackend {
    #[serde(default)]
    pub(crate) max_sessions: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawListener {
    pub(crate) name: String,
    pub(crate) bind: String,
    #[serde(default)]
    pub(crate) protocol: Protocol,
    /// Shorthand for a single `always` route. Mutually exclusive with `routes`.
    #[serde(default)]
    pub(crate) pool: Option<String>,
    /// Priority-ordered route rules; the first matching rule wins.
    #[serde(default)]
    pub(crate) routes: Vec<RawRoute>,
    /// UDP only: per-client → backend stickiness across session re-creation.
    #[serde(default)]
    pub(crate) affinity: Option<RawAffinity>,
    /// UDP only: serve a whole routed prefix on one wildcard socket, reading the
    /// real destination address per datagram (`IP_PKTINFO` / `IPV6_RECVPKTINFO`)
    /// and replying from it. `bind` must be a wildcard address. Datagrams whose
    /// destination falls outside the prefix are dropped.
    #[serde(default)]
    pub(crate) prefix: Option<String>,
    /// TCP only: set `IP_FREEBIND` / `IPV6_FREEBIND` so the listener can bind an
    /// address that is not (yet) configured on an interface.
    #[serde(default)]
    pub(crate) freebind: bool,
    /// TCP only (Linux): transparent mode. The listen socket is bound with
    /// `IP_TRANSPARENT` (accepts connections TPROXY-redirected to non-local
    /// addresses) and every upstream connection binds the real client address
    /// as its source, so the backend sees the client IP directly. Needs
    /// `CAP_NET_ADMIN` and policy routing that returns the backend's replies
    /// through this host — see `docs/04-transport-and-client-ip.md`.
    #[serde(default)]
    pub(crate) transparent: bool,
    /// Consult the push-resolver table (`POST /route-hint`) before the route
    /// list: a live `src_ip → pool` hint wins if its pool still exists.
    #[serde(default)]
    pub(crate) route_hint: bool,
    /// Filter chain, checked on the client source IP before routing. If `deny`
    /// matches, the connection / new datagram is dropped. If `allow` is
    /// non-empty, only source IPs it matches are admitted. `deny` wins over
    /// `allow`. CIDR strings (`10.0.0.0/8`, `2001:db8::/32`).
    #[serde(default)]
    pub(crate) allow: Vec<String>,
    #[serde(default)]
    pub(crate) deny: Vec<String>,
    /// UDP only: create a session only when the first datagram is positively
    /// recognised — a `sniffer` hint that is not `reject`, or a `first_bytes`
    /// route on this listener that matches it. Keeps generic spoof floods off
    /// the session table. Needs at least one `first_bytes` route or a `sniffer`.
    #[serde(default)]
    pub(crate) first_packet_gate: bool,
    /// GeoIP country filter on the client source IP, checked after the CIDR ACL.
    /// Needs `settings.geo_db`. `deny` wins; a non-empty `allow` is default-deny.
    #[serde(default)]
    pub(crate) geo: Option<RawGeo>,
    /// Cap on concurrent connections / UDP sessions per client IP and/or /24
    /// (v4) / /64 (v6). Refused before allocation once reached.
    #[serde(default)]
    pub(crate) per_source: Option<RawPerSource>,
    /// Token-bucket rate limit on new connections / new UDP sessions, keyed by
    /// source IP and/or /24 (v4) / /64 (v6). Checked after the ACL, before
    /// routing. Excess is dropped silently (no reflection).
    #[serde(default)]
    pub(crate) rate_limit: Option<RawRateLimit>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawRoute {
    pub(crate) r#match: RawMatch,
    pub(crate) action: RawAction,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawMatch {
    #[serde(rename = "type")]
    pub(crate) kind: String,
    /// `client_cidr` (source IP) / `dst` (destination IP): prefix list.
    #[serde(default)]
    pub(crate) cidrs: Option<Vec<String>>,
    /// `port` only: destination ports (bare int `30001` or `"lo-hi"` range).
    #[serde(default)]
    pub(crate) ports: Option<Vec<RawPort>>,
    /// `first_bytes` only: `"hex:ffffffff"` or `"ascii:hello"`.
    #[serde(default)]
    pub(crate) prefix: Option<String>,
    /// `first_bytes` only: observed first-bytes length must fall in this range.
    #[serde(default)]
    pub(crate) length: Option<RawLen>,
    /// `sni` / `sniffer`: host patterns — exact, `*.suffix` or `.suffix`.
    #[serde(default)]
    pub(crate) host: Option<Vec<String>>,
    /// `sniffer` only: the plugin name (see `KNOWN_SNIFFERS`).
    #[serde(default)]
    pub(crate) sniffer: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawLen {
    pub(crate) min: usize,
    pub(crate) max: usize,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum RawPort {
    Single(u32),
    Range(String),
}

/// A route action: exactly one of `pool` (fixed) or `resolver` (external lookup).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawAction {
    #[serde(default)]
    pub(crate) pool: Option<String>,
    #[serde(default)]
    pub(crate) resolver: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawResolver {
    pub(crate) name: String,
    #[serde(rename = "type", default = "default_resolver_type")]
    pub(crate) kind: String,
    pub(crate) endpoint: String,
    #[serde(default = "default_resolver_timeout_ms")]
    pub(crate) timeout_ms: u64,
    #[serde(default)]
    pub(crate) on_error: OnError,
    #[serde(default)]
    pub(crate) cache: Option<RawCache>,
    /// PROXY protocol header to prepend when this resolver returns a `target`
    /// (a pool-less connect, so there is no pool setting to read). A
    /// resolver-chosen *pool* uses that pool's own `proxy_protocol`.
    #[serde(default)]
    pub(crate) proxy_protocol: ProxyProtocol,
    /// Connect / idle timeout for a `target` result (a pool-less connect — no
    /// pool to read `connect_timeout_ms` / `idle_timeout_sec` from). A
    /// resolver-chosen *pool* uses that pool's own timeouts.
    #[serde(default = "default_target_connect_timeout_ms")]
    pub(crate) target_connect_timeout_ms: u64,
    #[serde(default = "default_target_idle_timeout_sec")]
    pub(crate) target_idle_timeout_sec: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawCache {
    /// Key parts, joined to form the cache key: `src_ip` | `src_ip_port` |
    /// `sni` | `routing_key` | `first_bytes:<a>:<b>`.
    pub(crate) key: Vec<String>,
    #[serde(default = "default_positive_ttl_sec")]
    pub(crate) positive_ttl_sec: u64,
    #[serde(default = "default_negative_ttl_sec")]
    pub(crate) negative_ttl_sec: u64,
    #[serde(default = "default_cache_max_entries")]
    pub(crate) max_entries: usize,
}

pub(crate) fn default_positive_ttl_sec() -> u64 {
    30
}
pub(crate) fn default_negative_ttl_sec() -> u64 {
    2
}
pub(crate) fn default_cache_max_entries() -> usize {
    10_000
}

pub(crate) fn default_resolver_type() -> String {
    "http".to_string()
}
pub(crate) fn default_resolver_timeout_ms() -> u64 {
    40
}
pub(crate) fn default_target_connect_timeout_ms() -> u64 {
    300
}
pub(crate) fn default_target_idle_timeout_sec() -> u64 {
    90
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawAffinity {
    #[serde(default)]
    pub(crate) hash_on: HashOn,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawGeo {
    #[serde(default)]
    pub(crate) allow: Vec<String>,
    #[serde(default)]
    pub(crate) deny: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawPerSource {
    /// Max concurrent connections / UDP sessions from one client IP.
    #[serde(default)]
    pub(crate) max_per_ip: Option<usize>,
    /// Max concurrent connections / UDP sessions from one /24 (v4) / /64 (v6).
    #[serde(default)]
    pub(crate) max_per_net: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawRateLimit {
    /// Token bucket per client source IP.
    #[serde(default)]
    pub(crate) per_ip: Option<RawBucket>,
    /// Token bucket per client /24 (IPv4) or /64 (IPv6) — catches distributed
    /// single-IP floods.
    #[serde(default)]
    pub(crate) per_net: Option<RawBucket>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawBucket {
    /// Sustained rate in permits per second (new connections / new UDP
    /// sessions). Must be >= 1.
    pub(crate) rate: u32,
    /// Bucket capacity (max burst). Defaults to `rate` (one second of slack).
    #[serde(default)]
    pub(crate) burst: Option<u32>,
}
