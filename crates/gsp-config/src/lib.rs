//! Configuration types, parsing and validation for the game server proxy.
//!
//! This is the reduced **v0 schema**: TCP/UDP listeners with a priority-ordered
//! route rule list (`always` / `client_cidr` / `dst` / `port` / `first_bytes` /
//! `sni` matchers; `sniffer` for future plugins) onto static pools, with active
//! health checks, round-robin /
//! least-connections / consistent-hash balancing, per-backend session caps, and
//! UDP session affinity. The full target schema lives in
//! `docs/05-configuration.md` and grows into this crate incrementally.

use std::collections::BTreeSet;
use std::net::{IpAddr, SocketAddr};
use std::ops::RangeInclusive;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse YAML: {0}")]
    Parse(#[from] serde_yaml::Error),
    #[error("invalid configuration: {0}")]
    Invalid(String),
}

// ---------------------------------------------------------------------------
// Raw types: a 1:1 mapping of the YAML document.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    settings: RawSettings,
    #[serde(default)]
    backend_sources: Vec<RawBackendSource>,
    #[serde(default)]
    pools: Vec<RawPool>,
    #[serde(default)]
    resolvers: Vec<RawResolver>,
    #[serde(default)]
    listeners: Vec<RawListener>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSettings {
    /// Accept-loop tasks per listener. `0` means "one per CPU core".
    #[serde(default)]
    workers: usize,
    /// Grace period for in-flight connections on shutdown, in seconds.
    #[serde(default = "default_shutdown_grace_sec")]
    shutdown_grace_sec: u64,
    #[serde(default)]
    admin: RawAdmin,
    /// Process-wide caps (phase 7). Startup-only (like `workers`).
    #[serde(default)]
    limits: RawLimits,
    /// Path to a MaxMind Country `.mmdb`, required when any listener has a `geo`
    /// filter. Startup-only.
    #[serde(default)]
    geo_db: Option<String>,
    /// Sniffer plugin loader (phase 9). Absent ⇒ no plugins load; a `sniffer:`
    /// route never matches. Startup-only for `dir` itself (rescanned on
    /// reload once the loader lands — phase 9 slice 4).
    #[serde(default)]
    sniffers: Option<RawSniffers>,
    /// Tier-2 regional health fabric (phase 13, docs/10 "Tier 2"). This
    /// instance's reachability-equivalence class. Must be set together with
    /// `gossip`, or not at all. Startup-only.
    #[serde(default)]
    failure_domain: Option<String>,
    /// Tier-2 gossip mesh membership (phase 13). Must be set together with
    /// `failure_domain`, or not at all. Startup-only.
    #[serde(default)]
    gossip: Option<RawGossip>,
    /// Self-reported fleet organization path (e.g. `"eu/frankfurt/cluster-a"`),
    /// pushed to gsp-aggregator alongside this instance's `IngestPayload` so
    /// the admin GUI can render a grouped/tree view. Purely a fleet-display
    /// label — never consulted by routing/forwarding. `/`-separated,
    /// non-empty segments, no leading/trailing `/`.
    #[serde(default)]
    group: Option<String>,
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
struct RawGossip {
    /// UDP address this instance's gossip mesh listens/sends on.
    bind: String,
    /// Known peers to bootstrap membership from (any subset of the domain's
    /// live members is enough — foca's own SWIM traffic discovers the rest).
    #[serde(default)]
    seeds: Vec<String>,
    /// Fraction of the domain's known members that must report a backend
    /// down for that verdict to override this instance's own "up" reading.
    /// Must be > 0.5 and <= 1.0 (a same-or-under-half quorum could contradict
    /// itself between two overlapping majorities).
    #[serde(default = "default_gossip_quorum_fraction")]
    quorum_fraction: f64,
    /// Pre-shared key: every gossip datagram carries an HMAC-SHA256 tag
    /// computed with it. A wrong or missing tag is dropped silently.
    psk: String,
}

fn default_gossip_quorum_fraction() -> f64 {
    0.66
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSniffers {
    /// Directory scanned for `*.wasm` plugin modules.
    dir: String,
    /// Per-call wall-clock budget (epoch interruption traps a plugin that
    /// overruns this).
    #[serde(default = "default_sniffer_call_timeout_ms")]
    call_timeout_ms: u64,
    /// Per-call memory ceiling for a plugin instance.
    #[serde(default = "default_sniffer_max_memory_bytes")]
    max_memory_bytes: usize,
    /// Optional supply-chain pin: a module whose file name isn't listed here,
    /// or whose SHA-256 doesn't match, is refused at load time.
    #[serde(default)]
    modules: Vec<RawSnifferModule>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSnifferModule {
    name: String,
    sha256: String,
    /// Opaque per-plugin configuration string, handed to the module on every
    /// `sniff` call (the plugin parses it however it likes — e.g. a pattern for
    /// `regex-firstbytes`). Omitted ⇒ the plugin gets an empty config.
    #[serde(default)]
    config: Option<String>,
}

fn default_sniffer_call_timeout_ms() -> u64 {
    20
}

fn default_sniffer_max_memory_bytes() -> usize {
    16 * 1024 * 1024
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLimits {
    /// Max live proxied TCP connections across all listeners. Absent ⇒ no cap.
    #[serde(default)]
    max_connections: Option<usize>,
    /// Max live UDP sessions across all listeners. Absent ⇒ no cap.
    #[serde(default)]
    max_udp_sessions: Option<usize>,
    /// Max new connections + UDP sessions accepted per second (token bucket,
    /// burst = the rate). Absent ⇒ no cap.
    #[serde(default)]
    max_new_sessions_per_sec: Option<u32>,
}

fn default_shutdown_grace_sec() -> u64 {
    30
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAdmin {
    #[serde(default = "default_admin_listen")]
    listen: String,
    /// Bearer token every admin API request (except `GET /healthz`) must
    /// present. `None` leaves the API open — network-boundary-only auth,
    /// same posture as always. Needed once anything calls in from outside
    /// that boundary — today, `gsp-aggregator`'s intent-verb fan-out
    /// (phase 10+11 slice 10, `docs/10` "The aggregator").
    #[serde(default)]
    auth_token: Option<String>,
}

impl Default for RawAdmin {
    fn default() -> Self {
        Self {
            listen: default_admin_listen(),
            auth_token: None,
        }
    }
}

fn default_admin_listen() -> String {
    "127.0.0.1:9900".to_string()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPool {
    name: String,
    /// Static backend list. Mutually exclusive with `source`.
    #[serde(default)]
    targets: Vec<String>,
    /// Name of a `backend_sources[]` entry that discovers this pool's backends.
    /// Mutually exclusive with `targets`.
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    balancer: Balancer,
    /// `consistent_hash` only: which part of the client address is the hash key.
    #[serde(default)]
    hash_on: Option<HashOn>,
    /// `weighted` only: `"ip:port"` → weight (>= 1). A backend (static or
    /// discovered) not listed here weighs `1`.
    #[serde(default)]
    weights: std::collections::HashMap<String, u32>,
    #[serde(default = "default_connect_timeout_ms")]
    connect_timeout_ms: u64,
    #[serde(default = "default_idle_timeout_sec")]
    idle_timeout_sec: u64,
    #[serde(default)]
    health_check: RawHealthCheck,
    #[serde(default)]
    per_backend: RawPerBackend,
    /// Prepend a PROXY protocol header to the upstream connection.
    #[serde(default)]
    proxy_protocol: ProxyProtocol,
}

fn default_connect_timeout_ms() -> u64 {
    300
}

fn default_idle_timeout_sec() -> u64 {
    90
}

/// A named backend discovery source (phase 8). Level-triggered: an adapter
/// returns the *current* address set for the pool; the runtime diffs it against
/// the live set. `static` is resolved to the pool's `targets` at validation
/// time; the other kinds get a control-plane refresh task in `gsp-core`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBackendSource {
    name: String,
    #[serde(rename = "type")]
    kind: String,
    /// How often the refresh task re-queries the source (dynamic kinds only).
    #[serde(default = "default_source_refresh_sec")]
    refresh_interval_sec: u64,
    /// `static` only: the fixed address list.
    #[serde(default)]
    targets: Vec<String>,
    /// `dns_srv` only: the SRV record to resolve (port comes from the record).
    #[serde(default)]
    record: Option<String>,
    /// `consul` / `kubernetes` only: the service name.
    #[serde(default)]
    service: Option<String>,
    /// `consul` only: base URL of the Consul HTTP API.
    #[serde(default)]
    consul_addr: Option<String>,
    /// `consul` only: restrict to service instances carrying this tag.
    #[serde(default)]
    tag: Option<String>,
    /// `kubernetes` only: the namespace (default `default`).
    #[serde(default)]
    namespace: Option<String>,
    /// `kubernetes` only: pick this named port from the Endpoints subset;
    /// absent ⇒ the first port.
    #[serde(default)]
    port_name: Option<String>,
    /// `kubernetes` only: API server base URL (default the in-cluster address).
    #[serde(default)]
    api: Option<String>,
    /// `tunnel` only: the origin's WireGuard public key (base64, 32 bytes) —
    /// identifies which `gsp-agent`-registered origin this source resolves
    /// backends from, and pins the key `gsp` expects that origin to present.
    #[serde(default)]
    pubkey: Option<String>,
}

fn default_source_refresh_sec() -> u64 {
    15
}

/// Minimal standard-alphabet base64 decoder, just enough to validate a
/// WireGuard key (32 bytes, i.e. exactly 44 chars with one trailing `=`).
/// `gsp-config` may only depend on `serde`/`serde_yaml`/`thiserror` (see
/// AGENTS.md's crate-boundary rule), so this doesn't pull in a `base64` crate
/// for one validation check. `pub` (not just used by this crate's own
/// `validate()`) so `gsp-controller`'s phase 14 backend-peers registry
/// (slice 2) can apply the exact same pubkey check without duplicating it —
/// `gsp-controller` already depends on `gsp-config` for `parse_str`.
pub fn base64_decode_32(s: &str) -> Option<[u8; 32]> {
    fn val(b: u8) -> Option<u8> {
        match b {
            b'A'..=b'Z' => Some(b - b'A'),
            b'a'..=b'z' => Some(b - b'a' + 26),
            b'0'..=b'9' => Some(b - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes = s.as_bytes();
    if bytes.len() != 44 || bytes[43] != b'=' {
        return None;
    }
    let mut out = [0u8; 32];
    for (chunk_idx, chunk) in bytes[..40].chunks(4).enumerate() {
        let vals: Vec<u8> = chunk.iter().map(|&b| val(b)).collect::<Option<_>>()?;
        let n = (vals[0] as u32) << 18
            | (vals[1] as u32) << 12
            | (vals[2] as u32) << 6
            | (vals[3] as u32);
        let o = chunk_idx * 3;
        out[o] = (n >> 16) as u8;
        out[o + 1] = (n >> 8) as u8;
        out[o + 2] = n as u8;
    }
    // Final 4-char group "XX==" style but here it's chars[40..44] = 3 data + '='.
    let last = &bytes[40..44];
    let v0 = val(last[0])?;
    let v1 = val(last[1])?;
    let v2 = val(last[2])?;
    if last[3] != b'=' {
        return None;
    }
    let n = (v0 as u32) << 18 | (v1 as u32) << 12 | (v2 as u32) << 6;
    out[30] = (n >> 16) as u8;
    out[31] = (n >> 8) as u8;
    Some(out)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHealthCheck {
    #[serde(rename = "type", default = "default_hc_type")]
    kind: String,
    #[serde(default = "default_hc_interval_sec")]
    interval_sec: u64,
    #[serde(default = "default_hc_timeout_ms")]
    timeout_ms: u64,
    #[serde(default = "default_hc_rise")]
    rise: u32,
    #[serde(default = "default_hc_fall")]
    fall: u32,
    /// `udp_probe` only: hex-encoded payload to send to the backend.
    #[serde(default)]
    send_hex: Option<String>,
    /// `udp_probe` only: hex-encoded prefix the reply must start with. Empty /
    /// absent means "any reply datagram counts as healthy".
    #[serde(default)]
    expect_hex_prefix: Option<String>,
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

fn default_hc_type() -> String {
    "tcp_connect".to_string()
}
fn default_hc_interval_sec() -> u64 {
    2
}
fn default_hc_timeout_ms() -> u64 {
    500
}
fn default_hc_rise() -> u32 {
    2
}
fn default_hc_fall() -> u32 {
    3
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawPerBackend {
    #[serde(default)]
    max_sessions: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawListener {
    name: String,
    bind: String,
    #[serde(default)]
    protocol: Protocol,
    /// Shorthand for a single `always` route. Mutually exclusive with `routes`.
    #[serde(default)]
    pool: Option<String>,
    /// Priority-ordered route rules; the first matching rule wins.
    #[serde(default)]
    routes: Vec<RawRoute>,
    /// UDP only: per-client → backend stickiness across session re-creation.
    #[serde(default)]
    affinity: Option<RawAffinity>,
    /// UDP only: serve a whole routed prefix on one wildcard socket, reading the
    /// real destination address per datagram (`IP_PKTINFO` / `IPV6_RECVPKTINFO`)
    /// and replying from it. `bind` must be a wildcard address. Datagrams whose
    /// destination falls outside the prefix are dropped.
    #[serde(default)]
    prefix: Option<String>,
    /// TCP only: set `IP_FREEBIND` / `IPV6_FREEBIND` so the listener can bind an
    /// address that is not (yet) configured on an interface.
    #[serde(default)]
    freebind: bool,
    /// TCP only (Linux): transparent mode. The listen socket is bound with
    /// `IP_TRANSPARENT` (accepts connections TPROXY-redirected to non-local
    /// addresses) and every upstream connection binds the real client address
    /// as its source, so the backend sees the client IP directly. Needs
    /// `CAP_NET_ADMIN` and policy routing that returns the backend's replies
    /// through this host — see `docs/04-transport-and-client-ip.md`.
    #[serde(default)]
    transparent: bool,
    /// Consult the push-resolver table (`POST /route-hint`) before the route
    /// list: a live `src_ip → pool` hint wins if its pool still exists.
    #[serde(default)]
    route_hint: bool,
    /// Filter chain, checked on the client source IP before routing. If `deny`
    /// matches, the connection / new datagram is dropped. If `allow` is
    /// non-empty, only source IPs it matches are admitted. `deny` wins over
    /// `allow`. CIDR strings (`10.0.0.0/8`, `2001:db8::/32`).
    #[serde(default)]
    allow: Vec<String>,
    #[serde(default)]
    deny: Vec<String>,
    /// UDP only: create a session only when the first datagram is positively
    /// recognised — a `sniffer` hint that is not `reject`, or a `first_bytes`
    /// route on this listener that matches it. Keeps generic spoof floods off
    /// the session table. Needs at least one `first_bytes` route or a `sniffer`.
    #[serde(default)]
    first_packet_gate: bool,
    /// GeoIP country filter on the client source IP, checked after the CIDR ACL.
    /// Needs `settings.geo_db`. `deny` wins; a non-empty `allow` is default-deny.
    #[serde(default)]
    geo: Option<RawGeo>,
    /// Cap on concurrent connections / UDP sessions per client IP and/or /24
    /// (v4) / /64 (v6). Refused before allocation once reached.
    #[serde(default)]
    per_source: Option<RawPerSource>,
    /// Token-bucket rate limit on new connections / new UDP sessions, keyed by
    /// source IP and/or /24 (v4) / /64 (v6). Checked after the ACL, before
    /// routing. Excess is dropped silently (no reflection).
    #[serde(default)]
    rate_limit: Option<RawRateLimit>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRoute {
    r#match: RawMatch,
    action: RawAction,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMatch {
    #[serde(rename = "type")]
    kind: String,
    /// `client_cidr` (source IP) / `dst` (destination IP): prefix list.
    #[serde(default)]
    cidrs: Option<Vec<String>>,
    /// `port` only: destination ports (bare int `30001` or `"lo-hi"` range).
    #[serde(default)]
    ports: Option<Vec<RawPort>>,
    /// `first_bytes` only: `"hex:ffffffff"` or `"ascii:hello"`.
    #[serde(default)]
    prefix: Option<String>,
    /// `first_bytes` only: observed first-bytes length must fall in this range.
    #[serde(default)]
    length: Option<RawLen>,
    /// `sni` / `sniffer`: host patterns — exact, `*.suffix` or `.suffix`.
    #[serde(default)]
    host: Option<Vec<String>>,
    /// `sniffer` only: the plugin name (see `KNOWN_SNIFFERS`).
    #[serde(default)]
    sniffer: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLen {
    min: usize,
    max: usize,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawPort {
    Single(u32),
    Range(String),
}

/// A route action: exactly one of `pool` (fixed) or `resolver` (external lookup).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAction {
    #[serde(default)]
    pool: Option<String>,
    #[serde(default)]
    resolver: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawResolver {
    name: String,
    #[serde(rename = "type", default = "default_resolver_type")]
    kind: String,
    endpoint: String,
    #[serde(default = "default_resolver_timeout_ms")]
    timeout_ms: u64,
    #[serde(default)]
    on_error: OnError,
    #[serde(default)]
    cache: Option<RawCache>,
    /// PROXY protocol header to prepend when this resolver returns a `target`
    /// (a pool-less connect, so there is no pool setting to read). A
    /// resolver-chosen *pool* uses that pool's own `proxy_protocol`.
    #[serde(default)]
    proxy_protocol: ProxyProtocol,
    /// Connect / idle timeout for a `target` result (a pool-less connect — no
    /// pool to read `connect_timeout_ms` / `idle_timeout_sec` from). A
    /// resolver-chosen *pool* uses that pool's own timeouts.
    #[serde(default = "default_target_connect_timeout_ms")]
    target_connect_timeout_ms: u64,
    #[serde(default = "default_target_idle_timeout_sec")]
    target_idle_timeout_sec: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCache {
    /// Key parts, joined to form the cache key: `src_ip` | `src_ip_port` |
    /// `sni` | `routing_key` | `first_bytes:<a>:<b>`.
    key: Vec<String>,
    #[serde(default = "default_positive_ttl_sec")]
    positive_ttl_sec: u64,
    #[serde(default = "default_negative_ttl_sec")]
    negative_ttl_sec: u64,
    #[serde(default = "default_cache_max_entries")]
    max_entries: usize,
}

fn default_positive_ttl_sec() -> u64 {
    30
}
fn default_negative_ttl_sec() -> u64 {
    2
}
fn default_cache_max_entries() -> usize {
    10_000
}

fn default_resolver_type() -> String {
    "http".to_string()
}
fn default_resolver_timeout_ms() -> u64 {
    40
}
fn default_target_connect_timeout_ms() -> u64 {
    300
}
fn default_target_idle_timeout_sec() -> u64 {
    90
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAffinity {
    #[serde(default)]
    hash_on: HashOn,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGeo {
    #[serde(default)]
    allow: Vec<String>,
    #[serde(default)]
    deny: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPerSource {
    /// Max concurrent connections / UDP sessions from one client IP.
    #[serde(default)]
    max_per_ip: Option<usize>,
    /// Max concurrent connections / UDP sessions from one /24 (v4) / /64 (v6).
    #[serde(default)]
    max_per_net: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRateLimit {
    /// Token bucket per client source IP.
    #[serde(default)]
    per_ip: Option<RawBucket>,
    /// Token bucket per client /24 (IPv4) or /64 (IPv6) — catches distributed
    /// single-IP floods.
    #[serde(default)]
    per_net: Option<RawBucket>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBucket {
    /// Sustained rate in permits per second (new connections / new UDP
    /// sessions). Must be >= 1.
    rate: u32,
    /// Bucket capacity (max burst). Defaults to `rate` (one second of slack).
    #[serde(default)]
    burst: Option<u32>,
}

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

// ---------------------------------------------------------------------------
// Routing.
// ---------------------------------------------------------------------------

/// An IPv4 or IPv6 CIDR block, parsed from `addr/prefix`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cidr {
    base: IpAddr,
    prefix: u8,
}

impl Cidr {
    /// Parse `"10.0.0.0/8"` / `"2001:db8::/32"`.
    pub fn parse(s: &str) -> Result<Self, String> {
        let (addr, prefix) = s
            .split_once('/')
            .ok_or_else(|| format!("CIDR {s:?} is missing a '/prefix'"))?;
        let base: IpAddr = addr
            .parse()
            .map_err(|_| format!("CIDR {s:?} has an invalid IP address"))?;
        let prefix: u8 = prefix
            .parse()
            .map_err(|_| format!("CIDR {s:?} has an invalid prefix length"))?;
        let max = if base.is_ipv4() { 32 } else { 128 };
        if prefix > max {
            return Err(format!("CIDR {s:?} prefix /{prefix} exceeds /{max}"));
        }
        Ok(Self { base, prefix })
    }

    /// Does `ip` fall within this block? (No v4-in-v6 normalisation.)
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.base, ip) {
            (IpAddr::V4(b), IpAddr::V4(x)) => bits_match(&b.octets(), &x.octets(), self.prefix),
            (IpAddr::V6(b), IpAddr::V6(x)) => bits_match(&b.octets(), &x.octets(), self.prefix),
            _ => false,
        }
    }
}

fn bits_match(a: &[u8], b: &[u8], prefix: u8) -> bool {
    let full = (prefix / 8) as usize;
    if a[..full] != b[..full] {
        return false;
    }
    let rem = prefix % 8;
    if rem == 0 {
        return true;
    }
    let mask = 0xffu8 << (8 - rem);
    (a[full] & mask) == (b[full] & mask)
}

/// A set of CIDR prefixes with O(prefix-length) membership tests: a binary radix
/// trie over address bits, IPv4 and IPv6 kept separate. Built once (per listener
/// spawn, through [`Acl`]) so a large deny / allow list — bogons plus a threat
/// feed, thousands of entries — costs a bounded bit-walk per connection instead
/// of a linear scan.
#[derive(Debug, Clone, Default)]
pub struct CidrSet {
    v4: Option<Box<TrieNode>>,
    v6: Option<Box<TrieNode>>,
    len: usize,
}

#[derive(Debug, Clone, Default)]
struct TrieNode {
    /// A prefix ends here — every address that reaches this node is covered.
    terminal: bool,
    children: [Option<Box<TrieNode>>; 2],
}

fn nth_bit(octets: &[u8], i: usize) -> usize {
    ((octets[i / 8] >> (7 - (i % 8))) & 1) as usize
}

impl CidrSet {
    pub fn build(cidrs: &[Cidr]) -> Self {
        let mut set = Self::default();
        for c in cidrs {
            match c.base {
                IpAddr::V4(a) => Self::insert(&mut set.v4, &a.octets(), c.prefix),
                IpAddr::V6(a) => Self::insert(&mut set.v6, &a.octets(), c.prefix),
            }
            set.len += 1;
        }
        set
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn len(&self) -> usize {
        self.len
    }

    /// Does any prefix in the set cover `ip`?
    pub fn contains(&self, ip: IpAddr) -> bool {
        match ip {
            IpAddr::V4(a) => Self::walk(&self.v4, &a.octets()),
            IpAddr::V6(a) => Self::walk(&self.v6, &a.octets()),
        }
    }

    fn insert(root: &mut Option<Box<TrieNode>>, octets: &[u8], prefix: u8) {
        let mut cur = root
            .get_or_insert_with(|| Box::new(TrieNode::default()))
            .as_mut();
        for i in 0..prefix as usize {
            if cur.terminal {
                return; // already covered by a shorter prefix
            }
            cur = cur.children[nth_bit(octets, i)]
                .get_or_insert_with(|| Box::new(TrieNode::default()))
                .as_mut();
        }
        cur.terminal = true;
        cur.children = [None, None]; // this prefix subsumes anything longer
    }

    fn walk(root: &Option<Box<TrieNode>>, octets: &[u8]) -> bool {
        let mut node = match root {
            Some(n) => n.as_ref(),
            None => return false,
        };
        if node.terminal {
            return true; // a /0 in the set
        }
        for i in 0..octets.len() * 8 {
            match &node.children[nth_bit(octets, i)] {
                Some(c) => {
                    node = c;
                    if node.terminal {
                        return true;
                    }
                }
                None => return false,
            }
        }
        false
    }
}

/// Per-listener source-IP filter, checked before routing (phase 7). Empty =
/// admit everyone. `deny` is checked first and wins; a non-empty `allow` then
/// makes the listener default-deny for anything it does not cover. The `Cidr`
/// vecs are kept for equality (reload diffing) and display; matching goes
/// through the compiled [`CidrSet`]s.
#[derive(Debug, Clone, Default)]
pub struct Acl {
    pub allow: Vec<Cidr>,
    pub deny: Vec<Cidr>,
    allow_set: CidrSet,
    deny_set: CidrSet,
}

impl PartialEq for Acl {
    fn eq(&self, other: &Self) -> bool {
        self.allow == other.allow && self.deny == other.deny
    }
}
impl Eq for Acl {}

impl Acl {
    pub fn new(allow: Vec<Cidr>, deny: Vec<Cidr>) -> Self {
        let allow_set = CidrSet::build(&allow);
        let deny_set = CidrSet::build(&deny);
        Self {
            allow,
            deny,
            allow_set,
            deny_set,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.allow.is_empty() && self.deny.is_empty()
    }

    /// Is a connection / datagram from `ip` admitted?
    pub fn permits(&self, ip: IpAddr) -> bool {
        if self.deny_set.contains(ip) {
            return false;
        }
        if !self.allow.is_empty() && !self.allow_set.contains(ip) {
            return false;
        }
        true
    }
}

/// Per-listener GeoIP country filter (phase 7), checked after the CIDR [`Acl`]
/// on the client source IP. Codes are ISO 3166-1 alpha-2, upper-cased at parse.
/// Same precedence as `Acl`: `deny` wins; a non-empty `allow` is default-deny.
/// The country lookup itself lives in `gsp-core` (needs the MaxMind DB).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GeoAcl {
    pub allow: Vec<[u8; 2]>,
    pub deny: Vec<[u8; 2]>,
}

impl GeoAcl {
    /// Admit a client whose source IP resolves to `country` (`None` = the IP is
    /// not in the database). An unknown country is admitted only when there is
    /// no `allow` list to fail closed against.
    pub fn permits(&self, country: Option<[u8; 2]>) -> bool {
        match country {
            Some(cc) => {
                if self.deny.contains(&cc) {
                    return false;
                }
                if !self.allow.is_empty() && !self.allow.contains(&cc) {
                    return false;
                }
                true
            }
            None => self.allow.is_empty(),
        }
    }
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

/// A host pattern for the `sni` matcher. Parsed lowercase; `*.foo` and `.foo`
/// both become `Suffix(".foo")` (a proper-subdomain match).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostPattern {
    Exact(String),
    /// Includes the leading `.`; matches `host.ends_with(self)`.
    Suffix(String),
}

impl HostPattern {
    fn matches(&self, host: &str) -> bool {
        match self {
            HostPattern::Exact(e) => host == e,
            HostPattern::Suffix(s) => host.ends_with(s.as_str()),
        }
    }
}

/// Structured hints a sniffer plugin returns after inspecting a connection's
/// first bytes. Read-only: a sniffer never sees later bytes and never writes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RouteHint {
    /// A hostname the sniffer recognised (Minecraft handshake host, TLS SNI, …).
    pub host: Option<String>,
    /// An opaque affinity / routing key (fed to a sticky table later).
    pub key: Option<String>,
    /// The sniffer wants this connection / datagram rejected outright. `gsp-core`
    /// drops it before routing (TCP: `gsp_listener_connections_total{result=
    /// "sniffer_reject"}`; UDP: no session, no reply,
    /// `gsp_datagrams_dropped_total{reason="sniffer_reject"}`) — it does not fall
    /// through to a later route such as `always`.
    pub reject: bool,
}

/// A single route's match condition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Matcher {
    /// Catch-all.
    Always,
    /// Source IP within any of these prefixes.
    ClientCidr(Vec<Cidr>),
    /// Destination IP (the address the client connected to, from `getsockname` /
    /// the listener's bind) within any of these prefixes. Only distinguishes
    /// addresses the OS already delivers separately; a wildcard-prefix listener
    /// (`IP_PKTINFO`) that makes this useful for a whole routed prefix is a
    /// later slice.
    DstCidr(Vec<Cidr>),
    /// Destination port (from the accepting socket) within any of these ranges.
    DstPort(Vec<RangeInclusive<u16>>),
    /// The connection's first bytes (TCP peek / first UDP datagram): start with
    /// `prefix` (when non-empty) **and** have an observed length within `len`
    /// (when set). At least one condition is present. On TCP `len` sees only
    /// what one peek returned (coarse); on UDP it is the exact datagram length.
    /// `regex` / `sniffer` variants come later.
    FirstBytes {
        prefix: Vec<u8>,
        len: Option<RangeInclusive<usize>>,
    },
    /// The TLS ClientHello's SNI host matches one of these patterns (TCP only;
    /// TLS is peeked, not terminated).
    Sni(Vec<HostPattern>),
    /// The named sniffer plugin (see `gsp_core::sniff`) recognised the first
    /// bytes. With `host` patterns: also requires the hint's `host` to match one
    /// of them; empty `host` ⇒ matches on any recognition. A `reject` hint never
    /// reaches this matcher — `gsp-core` drops the connection / datagram before
    /// routing (see [`RouteHint::reject`]) — so the `!reject` guard below is
    /// belt-and-braces for a direct `route_for` caller. The name is not
    /// validated here — the proxy checks it against the loaded sniffers at
    /// listener start (an unknown name simply never matches).
    Sniffer {
        name: String,
        host: Vec<HostPattern>,
    },
}

/// Hard cap on how many leading bytes the TCP path will `MSG_PEEK` (and on the
/// size of the peek buffer). Large enough for a typical TLS ClientHello.
pub const PEEK_MAX: usize = 4096;

/// Hard cap on a `first_bytes` prefix length.
const FIRST_BYTES_PREFIX_MAX: usize = 512;

/// Everything a [`Matcher`] can look at. `first_bytes` is empty when the listener
/// has no byte matcher (so nothing was peeked) or the peer sent nothing yet.
pub struct MatchContext<'a> {
    pub src: SocketAddr,
    pub local: SocketAddr,
    pub first_bytes: &'a [u8],
    /// The listener's sniffer result, if it has a `sniffer` route and the
    /// plugin produced a hint. `gsp-core` fills this in before routing.
    pub sniff: Option<&'a RouteHint>,
}

impl Matcher {
    pub fn matches(&self, ctx: &MatchContext) -> bool {
        match self {
            Matcher::Always => true,
            Matcher::ClientCidr(cidrs) => cidrs.iter().any(|c| c.contains(ctx.src.ip())),
            Matcher::DstCidr(cidrs) => cidrs.iter().any(|c| c.contains(ctx.local.ip())),
            Matcher::DstPort(ranges) => ranges.iter().any(|r| r.contains(&ctx.local.port())),
            Matcher::FirstBytes { prefix, len } => {
                let b = ctx.first_bytes;
                (prefix.is_empty() || b.starts_with(prefix.as_slice()))
                    && len.as_ref().is_none_or(|r| r.contains(&b.len()))
            }
            Matcher::Sni(pats) => match extract_sni(ctx.first_bytes) {
                Some(host) => pats.iter().any(|p| p.matches(&host)),
                None => false,
            },
            Matcher::Sniffer { name: _, host } => match ctx.sniff {
                Some(hint) if !hint.reject => {
                    host.is_empty()
                        || hint
                            .host
                            .as_deref()
                            .is_some_and(|h| host.iter().any(|p| p.matches(h)))
                }
                _ => false,
            },
        }
    }

    /// Leading bytes this matcher needs to see (0 for address-only matchers).
    fn peek_len(&self) -> usize {
        match self {
            // For a length bound, one byte past the upper end is enough to tell
            // "within range" from "above range".
            Matcher::FirstBytes { prefix, len } => prefix.len().max(
                len.as_ref()
                    .map_or(0, |r| r.end().saturating_add(1).min(PEEK_MAX)),
            ),
            Matcher::Sni(_) | Matcher::Sniffer { .. } => PEEK_MAX,
            _ => 0,
        }
    }
}

/// Extract the SNI `host_name` from a TLS ClientHello at the start of `buf`.
/// Returns `None` if `buf` is not a ClientHello, is truncated, or carries no
/// SNI. The TCP listener reassembles a ClientHello split across TCP segments
/// before calling this (it re-peeks until the whole first TLS record is
/// buffered or the peek budget expires), so a `None` from a genuine truncation
/// means the client stalled mid-handshake past the budget — that route then
/// just does not match.
pub fn extract_sni(buf: &[u8]) -> Option<String> {
    struct Reader<'a> {
        b: &'a [u8],
        pos: usize,
    }
    impl<'a> Reader<'a> {
        fn new(b: &'a [u8]) -> Self {
            Self { b, pos: 0 }
        }
        fn remaining(&self) -> usize {
            self.b.len() - self.pos
        }
        fn take(&mut self, n: usize) -> Option<&'a [u8]> {
            let end = self.pos.checked_add(n)?;
            let s = self.b.get(self.pos..end)?;
            self.pos = end;
            Some(s)
        }
        fn u8(&mut self) -> Option<u8> {
            Some(self.take(1)?[0])
        }
        fn u16(&mut self) -> Option<usize> {
            let x = self.take(2)?;
            Some(u16::from_be_bytes([x[0], x[1]]) as usize)
        }
        fn u24(&mut self) -> Option<usize> {
            let x = self.take(3)?;
            Some(u32::from_be_bytes([0, x[0], x[1], x[2]]) as usize)
        }
    }

    let mut r = Reader::new(buf);
    if r.u8()? != 0x16 {
        return None; // not a handshake record
    }
    r.take(2)?; // record version
    let rec_len = r.u16()?;
    let rec = r.take(rec_len)?; // first record fragment

    let mut h = Reader::new(rec);
    if h.u8()? != 0x01 {
        return None; // not a ClientHello
    }
    let hs_len = h.u24()?;
    let body = h.take(hs_len)?;

    let mut c = Reader::new(body);
    c.take(2)?; // client_version
    c.take(32)?; // random
    let sid_len = c.u8()? as usize;
    c.take(sid_len)?;
    let cs_len = c.u16()?;
    c.take(cs_len)?;
    let comp_len = c.u8()? as usize;
    c.take(comp_len)?;
    let ext_total = c.u16()?;
    let exts = c.take(ext_total)?;

    let mut e = Reader::new(exts);
    while e.remaining() >= 4 {
        let ext_type = e.u16()?;
        let ext_len = e.u16()?;
        let ext_data = e.take(ext_len)?;
        if ext_type != 0x0000 {
            continue; // not server_name
        }
        let mut s = Reader::new(ext_data);
        let list_len = s.u16()?;
        let list = s.take(list_len)?;
        let mut l = Reader::new(list);
        while l.remaining() >= 3 {
            let name_type = l.u8()?;
            let name_len = l.u16()?;
            let name = l.take(name_len)?;
            if name_type == 0x00 {
                let host = std::str::from_utf8(name).ok()?.to_ascii_lowercase();
                return (!host.is_empty()).then_some(host);
            }
        }
        return None;
    }
    None
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
    /// Kubernetes Endpoints of a service (polled).
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

/// Internal: a `backend_sources[]` entry after validation — either folded to a
/// fixed list (`static`) or a runtime source spec (dynamic kinds).
enum ResolvedSource {
    Static(Vec<SocketAddr>),
    Dynamic(SourceConfig),
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
    /// The single sniffer plugin this listener's routes use (`None` if no
    /// `sniffer` route). `gsp-core` runs it once per connection before routing.
    pub sniffer: Option<String>,
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
        if ctx.sniff.is_some_and(|h| !h.reject) {
            return true;
        }
        self.routes
            .iter()
            .any(|r| matches!(r.matcher, Matcher::FirstBytes { .. }) && r.matcher.matches(ctx))
    }
}

// ---------------------------------------------------------------------------
// Entry points.
// ---------------------------------------------------------------------------

/// Read and validate a config file from disk.
pub fn load(path: &Path) -> Result<Config, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.display().to_string(),
        source,
    })?;
    parse_str(&text)
}

/// Parse and validate a config from a YAML string.
pub fn parse_str(text: &str) -> Result<Config, ConfigError> {
    let raw: RawConfig = serde_yaml::from_str(text)?;
    validate(raw)
}

fn validate(raw: RawConfig) -> Result<Config, ConfigError> {
    use ConfigError::Invalid;

    if raw.listeners.is_empty() {
        return Err(Invalid("at least one listener is required".into()));
    }

    let admin_listen = raw.settings.admin.listen.parse().map_err(|_| {
        Invalid(format!(
            "settings.admin.listen is not a valid socket address: {}",
            raw.settings.admin.listen
        ))
    })?;
    let admin_auth_token = raw.settings.admin.auth_token.clone();

    // Resolve `backend_sources`. A `static` source becomes a fixed address list;
    // the dynamic kinds become a `SourceConfig` for the runtime refresh task.
    let mut source_names = BTreeSet::new();
    let mut sources: std::collections::HashMap<String, ResolvedSource> =
        std::collections::HashMap::with_capacity(raw.backend_sources.len());
    for s in raw.backend_sources {
        if !source_names.insert(s.name.clone()) {
            return Err(Invalid(format!(
                "duplicate backend_sources name: {}",
                s.name
            )));
        }
        if s.refresh_interval_sec == 0 {
            return Err(Invalid(format!(
                "backend_sources {}: refresh_interval_sec must be > 0",
                s.name
            )));
        }
        let refresh_interval = Duration::from_secs(s.refresh_interval_sec);
        let resolved = match s.kind.as_str() {
            "static" => {
                if s.targets.is_empty() {
                    return Err(Invalid(format!(
                        "backend_sources {}: type static needs a non-empty targets list",
                        s.name
                    )));
                }
                let mut addrs = Vec::with_capacity(s.targets.len());
                for t in &s.targets {
                    addrs.push(t.parse().map_err(|_| {
                        Invalid(format!(
                            "backend_sources {}: target is not a valid socket address: {t}",
                            s.name
                        ))
                    })?);
                }
                ResolvedSource::Static(addrs)
            }
            "dns_srv" => {
                let record = s.record.clone().ok_or_else(|| {
                    Invalid(format!(
                        "backend_sources {}: type dns_srv needs `record`",
                        s.name
                    ))
                })?;
                if record.is_empty() {
                    return Err(Invalid(format!(
                        "backend_sources {}: `record` must not be empty",
                        s.name
                    )));
                }
                ResolvedSource::Dynamic(SourceConfig {
                    name: s.name.clone(),
                    kind: SourceKind::DnsSrv { record },
                    refresh_interval,
                })
            }
            "consul" => {
                let service = s.service.clone().ok_or_else(|| {
                    Invalid(format!(
                        "backend_sources {}: type consul needs `service`",
                        s.name
                    ))
                })?;
                ResolvedSource::Dynamic(SourceConfig {
                    name: s.name.clone(),
                    kind: SourceKind::Consul {
                        service,
                        addr: s
                            .consul_addr
                            .clone()
                            .unwrap_or_else(|| "http://127.0.0.1:8500".to_string()),
                        tag: s.tag.clone(),
                    },
                    refresh_interval,
                })
            }
            "kubernetes" => {
                let service = s.service.clone().ok_or_else(|| {
                    Invalid(format!(
                        "backend_sources {}: type kubernetes needs `service`",
                        s.name
                    ))
                })?;
                ResolvedSource::Dynamic(SourceConfig {
                    name: s.name.clone(),
                    kind: SourceKind::Kubernetes {
                        namespace: s.namespace.clone().unwrap_or_else(|| "default".to_string()),
                        service,
                        port_name: s.port_name.clone(),
                        api: s
                            .api
                            .clone()
                            .unwrap_or_else(|| "https://kubernetes.default.svc".to_string()),
                    },
                    refresh_interval,
                })
            }
            "tunnel" => {
                let pubkey = s.pubkey.clone().ok_or_else(|| {
                    Invalid(format!(
                        "backend_sources {}: type tunnel needs `pubkey`",
                        s.name
                    ))
                })?;
                if base64_decode_32(&pubkey).is_none() {
                    return Err(Invalid(format!(
                        "backend_sources {}: `pubkey` must be a base64-encoded \
                         32-byte WireGuard key",
                        s.name
                    )));
                }
                ResolvedSource::Dynamic(SourceConfig {
                    name: s.name.clone(),
                    kind: SourceKind::Tunnel { pubkey },
                    refresh_interval,
                })
            }
            other => {
                return Err(Invalid(format!(
                    "backend_sources {}: unknown type {other:?} \
                     (static | dns_srv | consul | kubernetes | tunnel)",
                    s.name
                )))
            }
        };
        sources.insert(s.name, resolved);
    }

    let mut pool_names = BTreeSet::new();
    let mut pools = Vec::with_capacity(raw.pools.len());
    for p in raw.pools {
        if !pool_names.insert(p.name.clone()) {
            return Err(Invalid(format!("duplicate pool name: {}", p.name)));
        }
        // Backends come from either a static `targets` list or a named `source`.
        let (targets, source) = match &p.source {
            Some(_) if !p.targets.is_empty() => {
                return Err(Invalid(format!(
                    "pool {}: `targets` and `source` are mutually exclusive",
                    p.name
                )))
            }
            Some(src_name) => match sources.get(src_name) {
                None => {
                    return Err(Invalid(format!(
                        "pool {}: unknown backend_sources name: {src_name}",
                        p.name
                    )))
                }
                Some(ResolvedSource::Static(addrs)) => (addrs.clone(), None),
                Some(ResolvedSource::Dynamic(sc)) => (Vec::new(), Some(sc.clone())),
            },
            None => {
                if p.targets.is_empty() {
                    return Err(Invalid(format!(
                        "pool {}: needs `targets` or `source`",
                        p.name
                    )));
                }
                let mut targets = Vec::with_capacity(p.targets.len());
                for t in &p.targets {
                    let addr = t.parse().map_err(|_| {
                        Invalid(format!(
                            "pool {}: target is not a valid socket address: {t}",
                            p.name
                        ))
                    })?;
                    targets.push(addr);
                }
                (targets, None)
            }
        };
        if p.connect_timeout_ms == 0 {
            return Err(Invalid(format!(
                "pool {}: connect_timeout_ms must be > 0",
                p.name
            )));
        }

        let hc = &p.health_check;
        let hc_kind = match hc.kind.as_str() {
            "tcp_connect" => {
                if hc.send_hex.is_some() || hc.expect_hex_prefix.is_some() {
                    return Err(Invalid(format!(
                        "pool {}: send_hex / expect_hex_prefix only apply to health_check.type \
                         udp_probe",
                        p.name
                    )));
                }
                HealthCheckKind::TcpConnect
            }
            "udp_probe" => {
                let send = match &hc.send_hex {
                    Some(s) => parse_hex(s).map_err(|e| {
                        Invalid(format!("pool {}: health_check.send_hex: {e}", p.name))
                    })?,
                    None => {
                        return Err(Invalid(format!(
                            "pool {}: health_check.type udp_probe requires send_hex",
                            p.name
                        )))
                    }
                };
                if send.is_empty() {
                    return Err(Invalid(format!(
                        "pool {}: health_check.send_hex must not be empty",
                        p.name
                    )));
                }
                let expect_prefix = match &hc.expect_hex_prefix {
                    Some(s) => parse_hex(s).map_err(|e| {
                        Invalid(format!(
                            "pool {}: health_check.expect_hex_prefix: {e}",
                            p.name
                        ))
                    })?,
                    None => Vec::new(),
                };
                HealthCheckKind::UdpProbe {
                    send,
                    expect_prefix,
                }
            }
            other => {
                return Err(Invalid(format!(
                    "pool {}: health_check.type {other:?} is not supported \
                     (tcp_connect | udp_probe)",
                    p.name
                )))
            }
        };
        if hc.interval_sec == 0 || hc.timeout_ms == 0 {
            return Err(Invalid(format!(
                "pool {}: health_check interval_sec and timeout_ms must be > 0",
                p.name
            )));
        }
        if hc.rise == 0 || hc.fall == 0 {
            return Err(Invalid(format!(
                "pool {}: health_check rise and fall must be >= 1",
                p.name
            )));
        }
        if let Some(0) = p.per_backend.max_sessions {
            return Err(Invalid(format!(
                "pool {}: per_backend.max_sessions must be > 0 when set",
                p.name
            )));
        }

        let hash_on = match (p.balancer, p.hash_on) {
            (Balancer::ConsistentHash, on) => Some(on.unwrap_or(HashOn::SrcIp)),
            (_, Some(_)) => {
                return Err(Invalid(format!(
                    "pool {}: hash_on applies only to balancer consistent_hash",
                    p.name
                )))
            }
            (_, None) => None,
        };

        if !p.weights.is_empty() && p.balancer != Balancer::Weighted {
            return Err(Invalid(format!(
                "pool {}: weights applies only to balancer weighted",
                p.name
            )));
        }
        let mut weights = std::collections::HashMap::with_capacity(p.weights.len());
        for (addr, w) in &p.weights {
            if *w == 0 {
                return Err(Invalid(format!(
                    "pool {}: weight for {addr} must be >= 1",
                    p.name
                )));
            }
            let sa: SocketAddr = addr.parse().map_err(|_| {
                Invalid(format!(
                    "pool {}: weights key {addr:?} is not an ip:port",
                    p.name
                ))
            })?;
            weights.insert(sa, *w);
        }

        pools.push(PoolConfig {
            name: p.name,
            targets,
            source,
            balancer: p.balancer,
            hash_on,
            weights,
            connect_timeout: Duration::from_millis(p.connect_timeout_ms),
            idle_timeout: Duration::from_secs(p.idle_timeout_sec),
            health_check: HealthCheck {
                kind: hc_kind,
                interval: Duration::from_secs(hc.interval_sec),
                timeout: Duration::from_millis(hc.timeout_ms),
                rise: hc.rise,
                fall: hc.fall,
            },
            max_sessions: p.per_backend.max_sessions,
            proxy_protocol: p.proxy_protocol,
        });
    }

    let mut resolver_names = BTreeSet::new();
    let mut resolvers = Vec::with_capacity(raw.resolvers.len());
    for r in raw.resolvers {
        if !resolver_names.insert(r.name.clone()) {
            return Err(Invalid(format!("duplicate resolver name: {}", r.name)));
        }
        let kind = match r.kind.as_str() {
            "http" => ResolverKind::Http,
            "grpc" => ResolverKind::Grpc,
            other => {
                return Err(Invalid(format!(
                    "resolver {}: type {other:?} is not supported (http | grpc)",
                    r.name
                )))
            }
        };
        if r.endpoint.trim().is_empty() {
            return Err(Invalid(format!("resolver {}: endpoint is empty", r.name)));
        }
        if r.timeout_ms == 0 {
            return Err(Invalid(format!(
                "resolver {}: timeout_ms must be > 0",
                r.name
            )));
        }
        if r.target_connect_timeout_ms == 0 || r.target_idle_timeout_sec == 0 {
            return Err(Invalid(format!(
                "resolver {}: target_connect_timeout_ms and target_idle_timeout_sec must be > 0",
                r.name
            )));
        }
        let cache = match r.cache {
            Some(c) if !c.key.is_empty() => {
                let mut parts = Vec::with_capacity(c.key.len());
                for k in &c.key {
                    parts.push(parse_cache_key_part(&r.name, k)?);
                }
                if c.max_entries == 0 {
                    return Err(Invalid(format!(
                        "resolver {}: cache.max_entries must be > 0",
                        r.name
                    )));
                }
                Some(CacheConfig {
                    key: parts,
                    positive_ttl: Duration::from_secs(c.positive_ttl_sec),
                    negative_ttl: Duration::from_secs(c.negative_ttl_sec),
                    max_entries: c.max_entries,
                })
            }
            Some(_) => {
                return Err(Invalid(format!(
                    "resolver {}: cache.key must be non-empty",
                    r.name
                )))
            }
            None => None,
        };
        resolvers.push(ResolverConfig {
            name: r.name,
            kind,
            endpoint: r.endpoint,
            timeout: Duration::from_millis(r.timeout_ms),
            on_error: r.on_error,
            cache,
            proxy_protocol: r.proxy_protocol,
            target_connect_timeout: Duration::from_millis(r.target_connect_timeout_ms),
            target_idle_timeout: Duration::from_secs(r.target_idle_timeout_sec),
        });
    }

    let mut listener_names = BTreeSet::new();
    let mut binds = BTreeSet::new();
    let mut listeners = Vec::with_capacity(raw.listeners.len());
    for l in raw.listeners {
        if !listener_names.insert(l.name.clone()) {
            return Err(Invalid(format!("duplicate listener name: {}", l.name)));
        }
        let (bind, extra_binds) = parse_bind_spec(&l.name, &l.bind)?;
        for addr in std::iter::once(bind).chain(extra_binds.iter().copied()) {
            if !binds.insert((addr, l.protocol)) {
                return Err(Invalid(format!(
                    "listener {}: bind {addr} is already used by another listener",
                    l.name
                )));
            }
        }
        let routes = if !l.routes.is_empty() {
            if l.pool.is_some() {
                return Err(Invalid(format!(
                    "listener {}: set either `pool` or `routes`, not both",
                    l.name
                )));
            }
            let mut rs = Vec::with_capacity(l.routes.len());
            for (i, r) in l.routes.iter().enumerate() {
                let matcher = parse_matcher(&l.name, i, &r.r#match)?;
                let action = match (&r.action.pool, &r.action.resolver) {
                    (Some(p), None) => {
                        if !pool_names.contains(p) {
                            return Err(Invalid(format!(
                                "listener {}: route {i}: unknown pool {p}",
                                l.name
                            )));
                        }
                        Action::Pool(p.clone())
                    }
                    (None, Some(rn)) => {
                        if !resolver_names.contains(rn) {
                            return Err(Invalid(format!(
                                "listener {}: route {i}: unknown resolver {rn}",
                                l.name
                            )));
                        }
                        Action::Resolver(rn.clone())
                    }
                    _ => {
                        return Err(Invalid(format!(
                        "listener {}: route {i}: action needs exactly one of `pool` / `resolver`",
                        l.name
                    )))
                    }
                };
                rs.push(Route { matcher, action });
            }
            rs
        } else {
            let pool = l.pool.clone().ok_or_else(|| {
                Invalid(format!(
                    "listener {}: needs a `pool` or a `routes` list",
                    l.name
                ))
            })?;
            if !pool_names.contains(&pool) {
                return Err(Invalid(format!("listener {}: unknown pool {pool}", l.name)));
            }
            vec![Route {
                matcher: Matcher::Always,
                action: Action::Pool(pool),
            }]
        };
        if l.protocol == Protocol::Udp
            && routes.iter().any(|r| matches!(r.matcher, Matcher::Sni(_)))
        {
            return Err(Invalid(format!(
                "listener {}: the `sni` match requires a tcp listener",
                l.name
            )));
        }
        let affinity = match (l.protocol, l.affinity) {
            (Protocol::Tcp, Some(_)) => {
                return Err(Invalid(format!(
                    "listener {}: affinity applies only to udp listeners",
                    l.name
                )))
            }
            (Protocol::Tcp, None) => None,
            (Protocol::Udp, None) => Some(HashOn::default()),
            (Protocol::Udp, Some(a)) => Some(a.hash_on),
        };

        let prefix = match &l.prefix {
            Some(p) => {
                if l.protocol != Protocol::Udp {
                    return Err(Invalid(format!(
                        "listener {}: `prefix` mode requires a udp listener",
                        l.name
                    )));
                }
                if !extra_binds.is_empty() {
                    return Err(Invalid(format!(
                        "listener {}: `prefix` mode needs exactly one wildcard socket, not a \
                         bind port range",
                        l.name
                    )));
                }
                if !bind.ip().is_unspecified() {
                    return Err(Invalid(format!(
                        "listener {}: `prefix` mode needs a wildcard `bind` (e.g. \"[::]:{}\")",
                        l.name,
                        bind.port()
                    )));
                }
                let cidr = Cidr::parse(p)
                    .map_err(|e| Invalid(format!("listener {}: prefix: {e}", l.name)))?;
                Some(cidr)
            }
            None => None,
        };
        if l.freebind && l.protocol != Protocol::Tcp {
            return Err(Invalid(format!(
                "listener {}: `freebind` applies only to tcp listeners (udp uses `prefix`)",
                l.name
            )));
        }
        if l.transparent && l.prefix.is_some() {
            return Err(Invalid(format!(
                "listener {}: `transparent` and `prefix` are mutually exclusive (both derive \
                 the per-datagram destination, by different mechanisms)",
                l.name
            )));
        }
        if l.first_packet_gate {
            if l.protocol != Protocol::Udp {
                return Err(Invalid(format!(
                    "listener {}: `first_packet_gate` applies only to udp listeners",
                    l.name
                )));
            }
            let has_gateable = routes.iter().any(|r| {
                matches!(
                    r.matcher,
                    Matcher::FirstBytes { .. } | Matcher::Sniffer { .. }
                )
            });
            if !has_gateable {
                return Err(Invalid(format!(
                    "listener {}: `first_packet_gate` needs at least one `first_bytes` route \
                     or a `sniffer` (otherwise it drops every datagram)",
                    l.name
                )));
            }
        }

        let parse_cidrs = |field: &str, raw: &[String]| -> Result<Vec<Cidr>, ConfigError> {
            raw.iter()
                .map(|s| {
                    Cidr::parse(s)
                        .map_err(|e| Invalid(format!("listener {}: {field}: {e}", l.name)))
                })
                .collect()
        };
        let acl = Acl::new(
            parse_cidrs("allow", &l.allow)?,
            parse_cidrs("deny", &l.deny)?,
        );

        let geo = match &l.geo {
            None => None,
            Some(g) => {
                if raw.settings.geo_db.is_none() {
                    return Err(Invalid(format!(
                        "listener {}: `geo` needs `settings.geo_db` to be set",
                        l.name
                    )));
                }
                if g.allow.is_empty() && g.deny.is_empty() {
                    return Err(Invalid(format!(
                        "listener {}: `geo` needs a non-empty `allow` or `deny`",
                        l.name
                    )));
                }
                let parse_ccs = |field: &str,
                                 raw: &[String]|
                 -> Result<Vec<[u8; 2]>, ConfigError> {
                    raw.iter()
                        .map(|s| {
                            let b = s.as_bytes();
                            if b.len() == 2 && b.iter().all(u8::is_ascii_alphabetic) {
                                Ok([b[0].to_ascii_uppercase(), b[1].to_ascii_uppercase()])
                            } else {
                                Err(Invalid(format!(
                                    "listener {}: geo.{field}: {s:?} is not a 2-letter country code",
                                    l.name
                                )))
                            }
                        })
                        .collect()
                };
                Some(GeoAcl {
                    allow: parse_ccs("allow", &g.allow)?,
                    deny: parse_ccs("deny", &g.deny)?,
                })
            }
        };

        let per_source = match &l.per_source {
            None => None,
            Some(ps) => {
                for (field, v) in [
                    ("max_per_ip", ps.max_per_ip),
                    ("max_per_net", ps.max_per_net),
                ] {
                    if v == Some(0) {
                        return Err(Invalid(format!(
                            "listener {}: per_source.{field} must be >= 1 (omit for no cap)",
                            l.name
                        )));
                    }
                }
                if ps.max_per_ip.is_none() && ps.max_per_net.is_none() {
                    return Err(Invalid(format!(
                        "listener {}: `per_source` needs `max_per_ip` and/or `max_per_net`",
                        l.name
                    )));
                }
                Some(PerSourceLimit {
                    max_per_ip: ps.max_per_ip,
                    max_per_net: ps.max_per_net,
                })
            }
        };

        let rate_limit = match l.rate_limit {
            None => None,
            Some(rl) => {
                let to_bucket = |b: &RawBucket, which: &str| -> Result<TokenBucket, ConfigError> {
                    if b.rate == 0 {
                        return Err(Invalid(format!(
                            "listener {}: rate_limit.{which}.rate must be >= 1",
                            l.name
                        )));
                    }
                    let burst = b.burst.unwrap_or(b.rate).max(1);
                    Ok(TokenBucket {
                        rate: b.rate,
                        burst,
                    })
                };
                let per_ip = rl
                    .per_ip
                    .as_ref()
                    .map(|b| to_bucket(b, "per_ip"))
                    .transpose()?;
                let per_net = rl
                    .per_net
                    .as_ref()
                    .map(|b| to_bucket(b, "per_net"))
                    .transpose()?;
                if per_ip.is_none() && per_net.is_none() {
                    return Err(Invalid(format!(
                        "listener {}: rate_limit needs at least one of `per_ip` / `per_net`",
                        l.name
                    )));
                }
                Some(RateLimit { per_ip, per_net })
            }
        };

        // At most one sniffer plugin per listener (gsp-core runs one per conn).
        let mut sniffer: Option<String> = None;
        for r in &routes {
            if let Matcher::Sniffer { name, .. } = &r.matcher {
                match &sniffer {
                    Some(prev) if prev != name => {
                        return Err(Invalid(format!(
                            "listener {}: routes use two different sniffers ({prev}, {name}); \
                             only one per listener is supported",
                            l.name
                        )));
                    }
                    _ => sniffer = Some(name.clone()),
                }
            }
        }

        listeners.push(ListenerConfig {
            name: l.name,
            bind,
            extra_binds,
            protocol: l.protocol,
            routes,
            affinity,
            prefix,
            freebind: l.freebind,
            transparent: l.transparent,
            sniffer,
            route_hint: l.route_hint,
            first_packet_gate: l.first_packet_gate,
            acl,
            geo,
            per_source,
            rate_limit,
        });
    }

    // `proxy_protocol` form must match the transport of the listeners that use
    // the pool: v1/v2 are TCP-only, v2-udp is UDP-only. A pool statically routed
    // from both (or from the wrong transport) is rejected. Pools reached only
    // through a resolver `action` are not checked here (the target pool is not
    // known until runtime); the data path falls back to sending no header on a
    // transport mismatch.
    for p in &pools {
        if p.proxy_protocol == ProxyProtocol::None {
            continue;
        }
        let (mut on_tcp, mut on_udp) = (false, false);
        for l in &listeners {
            let uses = l
                .routes
                .iter()
                .any(|r| matches!(&r.action, Action::Pool(n) if n == &p.name));
            if uses {
                match l.protocol {
                    Protocol::Tcp => on_tcp = true,
                    Protocol::Udp => on_udp = true,
                }
            }
        }
        let want_udp = p.proxy_protocol == ProxyProtocol::V2Udp;
        if want_udp && on_tcp {
            return Err(Invalid(format!(
                "pool {}: proxy_protocol v2-udp is used by a TCP listener",
                p.name
            )));
        }
        if !want_udp && on_udp {
            return Err(Invalid(format!(
                "pool {}: proxy_protocol {} is used by a UDP listener (use v2-udp)",
                p.name,
                p.proxy_protocol.label()
            )));
        }
    }

    // Same transport check for a resolver's `target` PROXY protocol form:
    // v1/v2 are TCP-only, v2-udp is UDP-only. A resolver reached from listeners
    // of both transports (or the wrong one) is rejected.
    for r in &resolvers {
        if r.proxy_protocol == ProxyProtocol::None {
            continue;
        }
        let (mut on_tcp, mut on_udp) = (false, false);
        for l in &listeners {
            let uses = l
                .routes
                .iter()
                .any(|rt| matches!(&rt.action, Action::Resolver(n) if n == &r.name));
            if uses {
                match l.protocol {
                    Protocol::Tcp => on_tcp = true,
                    Protocol::Udp => on_udp = true,
                }
            }
        }
        let want_udp = r.proxy_protocol == ProxyProtocol::V2Udp;
        if want_udp && on_tcp {
            return Err(Invalid(format!(
                "resolver {}: proxy_protocol v2-udp is used by a TCP listener",
                r.name
            )));
        }
        if !want_udp && on_udp {
            return Err(Invalid(format!(
                "resolver {}: proxy_protocol {} is used by a UDP listener (use v2-udp)",
                r.name,
                r.proxy_protocol.label()
            )));
        }
    }

    let rl = &raw.settings.limits;
    for (name, zero) in [
        ("max_connections", rl.max_connections == Some(0)),
        ("max_udp_sessions", rl.max_udp_sessions == Some(0)),
        (
            "max_new_sessions_per_sec",
            rl.max_new_sessions_per_sec == Some(0),
        ),
    ] {
        if zero {
            return Err(Invalid(format!(
                "settings.limits.{name}: 0 blocks all traffic; omit the key for no cap"
            )));
        }
    }
    let limits = GlobalLimits {
        max_connections: rl.max_connections,
        max_udp_sessions: rl.max_udp_sessions,
        max_new_sessions_per_sec: rl.max_new_sessions_per_sec,
    };

    let sniffers = match raw.settings.sniffers {
        Some(rs) => Some(validate_sniffers(rs)?),
        None => None,
    };

    let gossip = match (raw.settings.failure_domain.clone(), raw.settings.gossip) {
        (None, None) => None,
        (Some(_), None) => {
            return Err(Invalid(
                "settings.failure_domain is set but settings.gossip is missing".into(),
            ))
        }
        (None, Some(_)) => {
            return Err(Invalid(
                "settings.gossip is set but settings.failure_domain is missing".into(),
            ))
        }
        (Some(_), Some(rg)) => Some(validate_gossip(rg)?),
    };

    let group = match raw.settings.group {
        Some(g) => Some(validate_group(g)?),
        None => None,
    };

    Ok(Config {
        workers: raw.settings.workers,
        shutdown_grace: Duration::from_secs(raw.settings.shutdown_grace_sec),
        admin_listen,
        admin_auth_token,
        pools,
        resolvers,
        listeners,
        limits,
        geo_db: raw.settings.geo_db,
        sniffers,
        failure_domain: raw.settings.failure_domain,
        gossip,
        group,
    })
}

fn validate_group(g: String) -> Result<String, ConfigError> {
    use ConfigError::Invalid;
    if g.is_empty() || g.starts_with('/') || g.ends_with('/') {
        return Err(Invalid(
            "settings.group must not be empty or start/end with '/'".into(),
        ));
    }
    if g.split('/').any(|segment| segment.is_empty()) {
        return Err(Invalid(
            "settings.group must not contain empty segments (e.g. \"a//b\")".into(),
        ));
    }
    Ok(g)
}

fn validate_gossip(rg: RawGossip) -> Result<GossipConfig, ConfigError> {
    use ConfigError::Invalid;
    let bind = rg.bind.parse().map_err(|_| {
        Invalid(format!(
            "settings.gossip.bind is not a valid socket address: {}",
            rg.bind
        ))
    })?;
    let mut seeds = Vec::with_capacity(rg.seeds.len());
    for s in &rg.seeds {
        seeds.push(s.parse().map_err(|_| {
            Invalid(format!(
                "settings.gossip.seeds: not a valid socket address: {s}"
            ))
        })?);
    }
    if !(rg.quorum_fraction > 0.5 && rg.quorum_fraction <= 1.0) {
        return Err(Invalid(
            "settings.gossip.quorum_fraction must be > 0.5 and <= 1.0".into(),
        ));
    }
    if rg.psk.is_empty() {
        return Err(Invalid("settings.gossip.psk must not be empty".into()));
    }
    Ok(GossipConfig {
        bind,
        seeds,
        quorum_fraction: rg.quorum_fraction,
        psk: rg.psk,
    })
}

fn validate_sniffers(rs: RawSniffers) -> Result<SniffersConfig, ConfigError> {
    use ConfigError::Invalid;
    if rs.dir.trim().is_empty() {
        return Err(Invalid("settings.sniffers.dir must not be empty".into()));
    }
    if rs.call_timeout_ms == 0 {
        return Err(Invalid(
            "settings.sniffers.call_timeout_ms must be > 0".into(),
        ));
    }
    if rs.max_memory_bytes == 0 {
        return Err(Invalid(
            "settings.sniffers.max_memory_bytes must be > 0".into(),
        ));
    }
    let mut modules = Vec::with_capacity(rs.modules.len());
    for m in rs.modules {
        if m.name.trim().is_empty() {
            return Err(Invalid(
                "settings.sniffers.modules[].name must not be empty".into(),
            ));
        }
        let hex_ok = m.sha256.len() == 64 && m.sha256.bytes().all(|b| b.is_ascii_hexdigit());
        if !hex_ok {
            return Err(Invalid(format!(
                "settings.sniffers.modules[{}].sha256 must be a 64-char hex digest",
                m.name
            )));
        }
        if matches!(&m.config, Some(c) if c.is_empty()) {
            return Err(Invalid(format!(
                "settings.sniffers.modules[{}].config must not be empty when set",
                m.name
            )));
        }
        modules.push(SnifferModulePin {
            name: m.name,
            sha256: m.sha256.to_ascii_lowercase(),
            config: m.config,
        });
    }
    Ok(SniffersConfig {
        dir: rs.dir,
        call_timeout: Duration::from_millis(rs.call_timeout_ms),
        max_memory_bytes: rs.max_memory_bytes,
        modules,
    })
}

fn parse_matcher(lname: &str, i: usize, m: &RawMatch) -> Result<Matcher, ConfigError> {
    use ConfigError::Invalid;
    let at = |s: String| Invalid(format!("listener {lname}: route {i}: {s}"));

    // Reject fields that do not belong to this match type.
    let allow = |field: &str, present: bool, allowed: bool| {
        if present && !allowed {
            Err(at(format!(
                "match type `{}` does not take `{field}`",
                m.kind
            )))
        } else {
            Ok(())
        }
    };
    allow(
        "cidrs",
        m.cidrs.is_some(),
        m.kind == "client_cidr" || m.kind == "dst",
    )?;
    allow("ports", m.ports.is_some(), m.kind == "port")?;
    allow("prefix", m.prefix.is_some(), m.kind == "first_bytes")?;
    allow("length", m.length.is_some(), m.kind == "first_bytes")?;
    allow(
        "host",
        m.host.is_some(),
        m.kind == "sni" || m.kind == "sniffer",
    )?;
    allow("sniffer", m.sniffer.is_some(), m.kind == "sniffer")?;

    // Optional (present or empty) host-pattern list, shared by `sni` / `sniffer`.
    let host_patterns = |min_one: bool| -> Result<Vec<HostPattern>, ConfigError> {
        match m.host.as_ref().filter(|h| !h.is_empty()) {
            Some(raw) => raw
                .iter()
                .map(|h| parse_host_pattern(h).map_err(at))
                .collect(),
            None if min_one => Err(at(format!(
                "match type `{}` needs a non-empty `host` list",
                m.kind
            ))),
            None => Ok(Vec::new()),
        }
    };

    match m.kind.as_str() {
        "always" => Ok(Matcher::Always),
        "client_cidr" | "dst" => {
            let raw = m.cidrs.as_ref().filter(|c| !c.is_empty()).ok_or_else(|| {
                at(format!(
                    "match type `{}` needs a non-empty `cidrs` list",
                    m.kind
                ))
            })?;
            let mut cidrs = Vec::with_capacity(raw.len());
            for c in raw {
                cidrs.push(Cidr::parse(c).map_err(at)?);
            }
            Ok(if m.kind == "dst" {
                Matcher::DstCidr(cidrs)
            } else {
                Matcher::ClientCidr(cidrs)
            })
        }
        "port" => {
            let raw = m
                .ports
                .as_ref()
                .filter(|p| !p.is_empty())
                .ok_or_else(|| at("match type `port` needs a non-empty `ports` list".into()))?;
            let mut ranges = Vec::with_capacity(raw.len());
            for p in raw {
                ranges.push(parse_port_range(lname, i, p)?);
            }
            Ok(Matcher::DstPort(ranges))
        }
        "first_bytes" => {
            let prefix = match &m.prefix {
                Some(s) => parse_byte_spec(s).map_err(at)?,
                None => Vec::new(),
            };
            if prefix.len() > FIRST_BYTES_PREFIX_MAX {
                return Err(at(format!(
                    "`first_bytes` prefix is {} bytes, over the {FIRST_BYTES_PREFIX_MAX}-byte limit",
                    prefix.len()
                )));
            }
            let len = match &m.length {
                Some(l) => {
                    if l.min > l.max {
                        return Err(at(format!(
                            "`first_bytes` length min {} is greater than max {}",
                            l.min, l.max
                        )));
                    }
                    Some(l.min..=l.max)
                }
                None => None,
            };
            if prefix.is_empty() && len.is_none() {
                return Err(at(
                    "match type `first_bytes` needs a `prefix` and/or a `length`".into(),
                ));
            }
            Ok(Matcher::FirstBytes { prefix, len })
        }
        "sni" => Ok(Matcher::Sni(host_patterns(true)?)),
        "sniffer" => {
            let name = m
                .sniffer
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| at("match type `sniffer` needs a `sniffer` name".into()))?;
            Ok(Matcher::Sniffer {
                name: name.to_string(),
                host: host_patterns(false)?,
            })
        }
        other => Err(at(format!(
            "unknown match type {other:?} \
             (always | client_cidr | dst | port | first_bytes | sni | sniffer)"
        ))),
    }
}

fn parse_cache_key_part(rname: &str, s: &str) -> Result<CacheKeyPart, ConfigError> {
    let bad = |m: String| ConfigError::Invalid(format!("resolver {rname}: cache.key: {m}"));
    match s {
        "src_ip" => Ok(CacheKeyPart::SrcIp),
        "src_ip_port" => Ok(CacheKeyPart::SrcIpPort),
        "sni" => Ok(CacheKeyPart::Sni),
        "routing_key" => Ok(CacheKeyPart::RoutingKey),
        _ => {
            let rest = s
                .strip_prefix("first_bytes:")
                .ok_or_else(|| bad(format!("unknown key part {s:?}")))?;
            let (a, b) = rest
                .split_once(':')
                .ok_or_else(|| bad(format!("{s:?} must be `first_bytes:<a>:<b>`")))?;
            let a: usize = a.parse().map_err(|_| bad(format!("bad start in {s:?}")))?;
            let b: usize = b.parse().map_err(|_| bad(format!("bad end in {s:?}")))?;
            if a > b {
                return Err(bad(format!("{s:?}: start > end")));
            }
            if b > PEEK_MAX {
                return Err(bad(format!("{s:?}: end exceeds the {PEEK_MAX}-byte peek")));
            }
            Ok(CacheKeyPart::FirstBytes(a..=b))
        }
    }
}

/// Parse an `sni` host pattern: exact (`eu.example.com`), or a subdomain suffix
/// written `*.eu.example.com` or `.eu.example.com`.
fn parse_host_pattern(p: &str) -> Result<HostPattern, String> {
    let p = p.trim().to_ascii_lowercase();
    if p.is_empty() {
        return Err("empty host pattern".into());
    }
    let body = p.strip_prefix("*.").or_else(|| p.strip_prefix('.'));
    match body {
        Some(rest) => {
            if rest.is_empty() || rest.contains('*') {
                return Err(format!("invalid host pattern {p:?}"));
            }
            Ok(HostPattern::Suffix(format!(".{rest}")))
        }
        None => {
            if p.contains('*') {
                return Err(format!(
                    "invalid host pattern {p:?} (`*` only as `*.suffix`)"
                ));
            }
            Ok(HostPattern::Exact(p))
        }
    }
}

/// Parse a `first_bytes` prefix: `"hex:ffff"` or `"ascii:text"`.
fn parse_byte_spec(s: &str) -> Result<Vec<u8>, String> {
    if let Some(h) = s.strip_prefix("hex:") {
        parse_hex(h)
    } else if let Some(a) = s.strip_prefix("ascii:") {
        Ok(a.as_bytes().to_vec())
    } else {
        Err(format!("prefix {s:?} must start with `hex:` or `ascii:`"))
    }
}

/// Max ports a single `bind: "host:lo-hi"` range may cover — one real socket
/// per port per worker, so an unbounded range risks fd exhaustion from a
/// typo (e.g. `0-65535`).
const MAX_BIND_RANGE: usize = 1024;

/// Parse a listener's `bind` string: either a plain `host:port` socket
/// address, or a `host:lo-hi` port range (requirement F1.4) — one socket per
/// port in the range, all sharing this listener's routes/filters/pool
/// selection (a route's `port` matcher still sees the real accepted/received
/// port). Returns the lowest port's address as the primary bind and the rest
/// (empty for a plain bind) as the extras.
fn parse_bind_spec(lname: &str, raw: &str) -> Result<(SocketAddr, Vec<SocketAddr>), ConfigError> {
    let bad = |s: String| ConfigError::Invalid(format!("listener {lname}: bind: {s}"));
    if let Ok(addr) = raw.parse::<SocketAddr>() {
        return Ok((addr, Vec::new()));
    }
    let invalid = || {
        bad(format!(
            "{raw:?} is not a valid socket address or port range"
        ))
    };
    let (host_part, port_part) = raw.rsplit_once(':').ok_or_else(invalid)?;
    let (lo, hi) = port_part.split_once('-').ok_or_else(invalid)?;
    let lo: u16 = lo.trim().parse().map_err(|_| {
        bad(format!(
            "bind range {raw:?} has an invalid lower port bound"
        ))
    })?;
    let hi: u16 = hi.trim().parse().map_err(|_| {
        bad(format!(
            "bind range {raw:?} has an invalid upper port bound"
        ))
    })?;
    if lo == 0 || hi == 0 {
        return Err(bad(format!("bind range {raw:?} includes port 0")));
    }
    if lo > hi {
        return Err(bad(format!("bind range {raw:?} is reversed (lo > hi)")));
    }
    let width = (hi - lo) as usize + 1;
    if width > MAX_BIND_RANGE {
        return Err(bad(format!(
            "bind range {raw:?} covers {width} ports, over the {MAX_BIND_RANGE} limit \
             (one socket per port, per worker)"
        )));
    }
    let host = host_part.trim_start_matches('[').trim_end_matches(']');
    let ip: IpAddr = host
        .parse()
        .map_err(|_| bad(format!("bind range {raw:?} has an invalid host {host:?}")))?;
    let mut addrs = (lo..=hi).map(|p| SocketAddr::new(ip, p));
    let first = addrs
        .next()
        .expect("lo..=hi is non-empty (lo <= hi checked above)");
    Ok((first, addrs.collect()))
}

fn parse_port_range(
    lname: &str,
    i: usize,
    p: &RawPort,
) -> Result<RangeInclusive<u16>, ConfigError> {
    let bad = |s: String| ConfigError::Invalid(format!("listener {lname}: route {i}: {s}"));
    match p {
        RawPort::Single(n) => {
            let n = u16::try_from(*n).map_err(|_| bad(format!("port {n} is out of range")))?;
            if n == 0 {
                return Err(bad("port 0 is not valid".into()));
            }
            Ok(n..=n)
        }
        RawPort::Range(s) => {
            let (lo, hi) = s
                .split_once('-')
                .ok_or_else(|| bad(format!("port range {s:?} must be `lo-hi`")))?;
            let lo: u16 = lo
                .trim()
                .parse()
                .map_err(|_| bad(format!("port range {s:?} has an invalid lower bound")))?;
            let hi: u16 = hi
                .trim()
                .parse()
                .map_err(|_| bad(format!("port range {s:?} has an invalid upper bound")))?;
            if lo == 0 || hi == 0 {
                return Err(bad(format!("port range {s:?} includes port 0")));
            }
            if lo > hi {
                return Err(bad(format!("port range {s:?} is reversed")));
            }
            Ok(lo..=hi)
        }
    }
}

/// Parse a hex string (optional ASCII whitespace between bytes) into bytes.
fn parse_hex(s: &str) -> Result<Vec<u8>, String> {
    let compact: String = s.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    if !compact.len().is_multiple_of(2) {
        return Err(format!("odd number of hex digits in {s:?}"));
    }
    (0..compact.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&compact[i..i + 2], 16)
                .map_err(|_| format!("invalid hex byte {:?}", &compact[i..i + 2]))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `MatchContext` from `"src" , "local"` strings and optional bytes.
    fn ctx<'a>(src: &str, local: &str, first_bytes: &'a [u8]) -> MatchContext<'a> {
        MatchContext {
            src: src.parse().unwrap(),
            local: local.parse().unwrap(),
            first_bytes,
            sniff: None,
        }
    }

    /// Like [`ctx`] but with a sniffer hint attached.
    fn ctx_sniff<'a>(hint: &'a RouteHint) -> MatchContext<'a> {
        MatchContext {
            src: "9.9.9.9:1".parse().unwrap(),
            local: "1.1.1.1:25565".parse().unwrap(),
            first_bytes: &[],
            sniff: Some(hint),
        }
    }

    const MINIMAL: &str = r#"
pools:
  - name: local
    targets: ["127.0.0.1:9001"]
listeners:
  - name: public-tcp
    bind: "0.0.0.0:7777"
    pool: local
"#;

    #[test]
    fn parses_minimal_config() {
        let cfg = parse_str(MINIMAL).expect("should parse");
        assert_eq!(cfg.listeners.len(), 1);
        assert_eq!(cfg.pools.len(), 1);
        assert_eq!(cfg.admin_listen.port(), 9900);
        assert_eq!(cfg.admin_auth_token, None);
        assert_eq!(cfg.pools[0].connect_timeout.as_millis(), 300);
        assert_eq!(cfg.pools[0].balancer, Balancer::RoundRobin);
        assert_eq!(cfg.pools[0].health_check.rise, 2);
        assert_eq!(cfg.pools[0].health_check.fall, 3);
        assert!(cfg.pools[0].max_sessions.is_none());
        assert_eq!(cfg.pools[0].proxy_protocol, ProxyProtocol::None);
    }

    #[test]
    fn parses_admin_auth_token() {
        let cfg = parse_str(
            r#"
settings:
  admin:
    listen: "127.0.0.1:9900"
    auth_token: "secret123"
pools:
  - name: p
    targets: ["127.0.0.1:9001"]
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    pool: p
"#,
        )
        .expect("should parse");
        assert_eq!(cfg.admin_auth_token.as_deref(), Some("secret123"));
    }

    #[test]
    fn parses_proxy_protocol_pool_option() {
        let cfg = parse_str(
            r#"
pools:
  - name: p
    targets: ["127.0.0.1:9001"]
    proxy_protocol: v2
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    pool: p
"#,
        )
        .expect("should parse");
        assert_eq!(cfg.pools[0].proxy_protocol, ProxyProtocol::V2);
    }

    #[test]
    fn parses_v2_udp_on_a_udp_listener() {
        let cfg = parse_str(
            r#"
pools:
  - name: p
    targets: ["127.0.0.1:9001"]
    proxy_protocol: v2-udp
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    protocol: udp
    pool: p
"#,
        )
        .expect("should parse");
        assert_eq!(cfg.pools[0].proxy_protocol, ProxyProtocol::V2Udp);
    }

    #[test]
    fn rejects_v2_udp_on_a_tcp_listener() {
        let err = parse_str(
            r#"
pools:
  - name: p
    targets: ["127.0.0.1:9001"]
    proxy_protocol: v2-udp
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    protocol: tcp
    pool: p
"#,
        );
        assert!(err.is_err());
    }

    #[test]
    fn rejects_tcp_proxy_protocol_on_a_udp_listener() {
        let err = parse_str(
            r#"
pools:
  - name: p
    targets: ["127.0.0.1:9001"]
    proxy_protocol: v2
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    protocol: udp
    pool: p
"#,
        );
        assert!(err.is_err());
    }

    #[test]
    fn rejects_unknown_proxy_protocol() {
        let err = parse_str(
            r#"
pools:
  - name: p
    targets: ["127.0.0.1:9001"]
    proxy_protocol: v3
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    pool: p
"#,
        );
        assert!(err.is_err());
    }

    #[test]
    fn parses_health_check_and_caps() {
        let yaml = r#"
pools:
  - name: p
    targets: ["127.0.0.1:1", "127.0.0.1:2"]
    balancer: least_conn
    health_check: { interval_sec: 1, timeout_ms: 200, rise: 1, fall: 1 }
    per_backend: { max_sessions: 50 }
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    pool: p
"#;
        let cfg = parse_str(yaml).unwrap();
        assert_eq!(cfg.pools[0].balancer, Balancer::LeastConn);
        assert_eq!(cfg.pools[0].health_check.interval.as_secs(), 1);
        assert_eq!(cfg.pools[0].health_check.fall, 1);
        assert_eq!(cfg.pools[0].max_sessions, Some(50));
    }

    #[test]
    fn parses_consistent_hash_balancer() {
        let yaml = r#"
pools:
  - name: a
    targets: ["127.0.0.1:1"]
    balancer: consistent_hash
  - name: b
    targets: ["127.0.0.1:2"]
    balancer: consistent_hash
    hash_on: src_ip_port
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    pool: a
"#;
        let cfg = parse_str(yaml).unwrap();
        assert_eq!(cfg.pools[0].balancer, Balancer::ConsistentHash);
        assert_eq!(cfg.pools[0].hash_on, Some(HashOn::SrcIp)); // default
        assert_eq!(cfg.pools[1].hash_on, Some(HashOn::SrcIpPort));
    }

    #[test]
    fn rejects_hash_on_without_consistent_hash() {
        let yaml = r#"
pools:
  - name: p
    targets: ["127.0.0.1:1"]
    balancer: round_robin
    hash_on: src_ip
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    pool: p
"#;
        assert!(parse_str(yaml).is_err());
    }

    #[test]
    fn parses_weighted_pool_and_rejects_bad_weights() {
        let yaml = r#"
pools:
  - name: p
    targets: ["127.0.0.1:1", "127.0.0.1:2"]
    balancer: weighted
    weights: { "127.0.0.1:1": 3, "127.0.0.1:2": 1 }
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    pool: p
"#;
        let cfg = parse_str(yaml).unwrap();
        assert_eq!(cfg.pools[0].balancer, Balancer::Weighted);
        assert_eq!(
            cfg.pools[0].weights.get(&"127.0.0.1:1".parse().unwrap()),
            Some(&3)
        );

        let bad = |extra: &str, bal: &str| {
            format!(
                "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n    balancer: {bal}\n    {extra}\nlisteners:\n  - {{ name: l, bind: \"0.0.0.0:7777\", pool: p }}\n"
            )
        };
        // weights with a non-weighted balancer
        assert!(parse_str(&bad(r#"weights: { "127.0.0.1:1": 2 }"#, "round_robin")).is_err());
        // weight of zero
        assert!(parse_str(&bad(r#"weights: { "127.0.0.1:1": 0 }"#, "weighted")).is_err());
        // weights key that isn't an ip:port
        assert!(parse_str(&bad(r#"weights: { "not-an-addr": 2 }"#, "weighted")).is_err());
    }

    #[test]
    fn rejects_unknown_pool() {
        let yaml = r#"
pools:
  - name: local
    targets: ["127.0.0.1:9001"]
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    pool: nope
"#;
        let err = parse_str(yaml).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(_)), "got {err:?}");
    }

    #[test]
    fn accepts_udp_listener_with_default_affinity() {
        let yaml = r#"
pools:
  - name: local
    targets: ["127.0.0.1:9001"]
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    protocol: udp
    pool: local
"#;
        let cfg = parse_str(yaml).unwrap();
        assert_eq!(cfg.listeners[0].protocol, Protocol::Udp);
        assert_eq!(cfg.listeners[0].affinity, Some(HashOn::SrcIp));
    }

    #[test]
    fn rejects_affinity_on_tcp_listener() {
        let yaml = r#"
pools:
  - name: local
    targets: ["127.0.0.1:9001"]
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    protocol: tcp
    pool: local
    affinity: { hash_on: src_ip }
"#;
        assert!(parse_str(yaml).is_err());
    }

    #[test]
    fn parses_udp_probe_health_check() {
        let yaml = r#"
pools:
  - name: p
    targets: ["127.0.0.1:1"]
    health_check: { type: udp_probe, send_hex: "ff ff ff ff", expect_hex_prefix: "ffff" }
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    protocol: udp
    pool: p
"#;
        let cfg = parse_str(yaml).unwrap();
        assert_eq!(
            cfg.pools[0].health_check.kind,
            HealthCheckKind::UdpProbe {
                send: vec![0xff, 0xff, 0xff, 0xff],
                expect_prefix: vec![0xff, 0xff],
            }
        );
    }

    #[test]
    fn rejects_udp_probe_without_send_hex() {
        let yaml = r#"
pools:
  - name: p
    targets: ["127.0.0.1:1"]
    health_check: { type: udp_probe }
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    protocol: udp
    pool: p
"#;
        assert!(parse_str(yaml).is_err());
    }

    #[test]
    fn rejects_zero_rise() {
        let yaml = r#"
pools:
  - name: p
    targets: ["127.0.0.1:1"]
    health_check: { rise: 0 }
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    pool: p
"#;
        assert!(parse_str(yaml).is_err());
    }

    #[test]
    fn rejects_zero_max_sessions() {
        let yaml = r#"
pools:
  - name: p
    targets: ["127.0.0.1:1"]
    per_backend: { max_sessions: 0 }
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    pool: p
"#;
        assert!(parse_str(yaml).is_err());
    }

    #[test]
    fn rejects_empty_targets() {
        let yaml = r#"
pools:
  - name: local
    targets: []
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    pool: local
"#;
        assert!(parse_str(yaml).is_err());
    }

    #[test]
    fn rejects_no_listeners() {
        assert!(parse_str("pools: []").is_err());
    }

    #[test]
    fn bare_pool_becomes_one_always_route() {
        let cfg = parse_str(MINIMAL).unwrap();
        assert_eq!(cfg.listeners[0].routes.len(), 1);
        assert_eq!(cfg.listeners[0].routes[0].matcher, Matcher::Always);
        assert_eq!(
            cfg.listeners[0].routes[0].action,
            Action::Pool("local".into())
        );
    }

    #[test]
    fn parses_route_list_and_matches_first() {
        let yaml = r#"
pools:
  - name: staging
    targets: ["127.0.0.1:1"]
  - name: prod
    targets: ["127.0.0.1:2"]
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    routes:
      - match: { type: client_cidr, cidrs: ["10.0.0.0/8", "192.168.1.0/24"] }
        action: { pool: staging }
      - match: { type: port, ports: [7777, "8000-8100"] }
        action: { pool: prod }
      - match: { type: always }
        action: { pool: prod }
"#;
        let cfg = parse_str(yaml).unwrap();
        let l = &cfg.listeners[0];
        assert_eq!(l.routes.len(), 3);
        assert_eq!(l.peek_len(), 0);
        assert_eq!(
            l.route_for(&ctx("10.1.2.3:5555", "203.0.113.1:7777", &[])),
            Some("staging")
        );
        assert_eq!(
            l.route_for(&ctx("192.168.1.9:5555", "203.0.113.1:7777", &[])),
            Some("staging")
        );
        // no CIDR match, but destination port 7777 does
        assert_eq!(
            l.route_for(&ctx("203.0.113.9:5555", "203.0.113.1:7777", &[])),
            Some("prod")
        );
        // falls through to `always`
        assert_eq!(
            l.route_for(&ctx("203.0.113.9:5555", "203.0.113.1:9999", &[])),
            Some("prod")
        );
    }

    #[test]
    fn dst_matcher_selects_by_destination_ip() {
        let yaml = r#"
pools:
  - name: survival
    targets: ["127.0.0.1:1"]
  - name: creative
    targets: ["127.0.0.1:2"]
  - name: lobby
    targets: ["127.0.0.1:3"]
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    routes:
      - match: { type: dst, cidrs: ["2001:db8:ace:1::1/128", "198.51.100.7/32"] }
        action: { pool: survival }
      - match: { type: dst, cidrs: ["2001:db8:ace:1::2/128"] }
        action: { pool: creative }
      - match: { type: always }
        action: { pool: lobby }
"#;
        let cfg = parse_str(yaml).unwrap();
        let l = &cfg.listeners[0];
        assert_eq!(l.peek_len(), 0);
        let on = |ip: &str| {
            l.route_for(&ctx("203.0.113.9:5555", &format!("{ip}:7777"), &[]))
                .map(str::to_string)
        };
        assert_eq!(on("198.51.100.7").as_deref(), Some("survival"));
        assert_eq!(on("[2001:db8:ace:1::1]").as_deref(), Some("survival"));
        assert_eq!(on("[2001:db8:ace:1::2]").as_deref(), Some("creative"));
        assert_eq!(on("198.51.100.9").as_deref(), Some("lobby"));
    }

    #[test]
    fn rejects_dst_without_cidrs_and_cidrs_on_wrong_type() {
        for bad in [
            r#"routes: [{ match: { type: dst }, action: { pool: p } }]"#,
            r#"routes: [{ match: { type: dst, cidrs: [] }, action: { pool: p } }]"#,
            r#"routes: [{ match: { type: port, cidrs: ["10.0.0.0/8"] }, action: { pool: p } }]"#,
        ] {
            let yaml = format!(
                "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\nlisteners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    {bad}\n"
            );
            assert!(parse_str(&yaml).is_err(), "should reject: {bad}");
        }
    }

    #[test]
    fn first_bytes_prefix_matches_and_sets_peek_len() {
        let yaml = r#"
pools:
  - name: query
    targets: ["127.0.0.1:1"]
  - name: game
    targets: ["127.0.0.1:2"]
listeners:
  - name: l
    bind: "0.0.0.0:27015"
    protocol: udp
    routes:
      - match: { type: first_bytes, prefix: "hex:ffffffff" }
        action: { pool: query }
      - match: { type: first_bytes, prefix: "ascii:GET " }
        action: { pool: query }
      - match: { type: always }
        action: { pool: game }
"#;
        let cfg = parse_str(yaml).unwrap();
        let l = &cfg.listeners[0];
        assert_eq!(l.peek_len(), 4);
        assert_eq!(
            l.route_for(&ctx(
                "1.2.3.4:5",
                "9.9.9.9:27015",
                &[0xff, 0xff, 0xff, 0xff, 0x54]
            )),
            Some("query")
        );
        assert_eq!(
            l.route_for(&ctx("1.2.3.4:5", "9.9.9.9:27015", b"GET /x")),
            Some("query")
        );
        assert_eq!(
            l.route_for(&ctx("1.2.3.4:5", "9.9.9.9:27015", b"\x01\x02random")),
            Some("game")
        );
        // nothing peeked yet -> prefix routes cannot match, falls through
        assert_eq!(
            l.route_for(&ctx("1.2.3.4:5", "9.9.9.9:27015", &[])),
            Some("game")
        );
    }

    #[test]
    fn first_bytes_length_bound_matches_and_combines_with_prefix() {
        let yaml = r#"
pools:
  - name: q
    targets: ["127.0.0.1:1"]
  - name: g
    targets: ["127.0.0.1:2"]
listeners:
  - name: l
    bind: "0.0.0.0:27015"
    protocol: udp
    routes:
      - match: { type: first_bytes, prefix: "hex:ffff", length: { min: 4, max: 8 } }
        action: { pool: q }
      - match: { type: first_bytes, length: { min: 0, max: 15 } }
        action: { pool: q }
      - match: { type: always }
        action: { pool: g }
"#;
        let cfg = parse_str(yaml).unwrap();
        let l = &cfg.listeners[0];
        assert_eq!(l.peek_len(), 16); // max(prefix 2, rule1 8+1, rule2 15+1)
        let route = |b: &[u8]| l.route_for(&ctx("1.2.3.4:5", "9.9.9.9:27015", b));
        // rule 1: prefix ffff AND length 4..=8
        assert_eq!(route(&[0xff, 0xff, 0x01, 0x02, 0x03]), Some("q"));
        // prefix ok but length 2 out of 4..=8 -> rule 1 skipped, rule 2 (len<=15) hits
        assert_eq!(route(&[0xff, 0xff]), Some("q"));
        // length 20 > 15 and no ffff prefix -> nothing but `always`
        assert_eq!(route(&[0u8; 20]), Some("g"));
    }

    #[test]
    fn rejects_bad_first_bytes_specs() {
        for bad in [
            r#"routes: [{ match: { type: first_bytes, prefix: "ffff" }, action: { pool: p } }]"#,
            r#"routes: [{ match: { type: first_bytes }, action: { pool: p } }]"#,
            r#"routes: [{ match: { type: first_bytes, prefix: "hex:" }, action: { pool: p } }]"#,
            r#"routes: [{ match: { type: first_bytes, length: { min: 9, max: 4 } }, action: { pool: p } }]"#,
            r#"routes: [{ match: { type: always, prefix: "hex:ff" }, action: { pool: p } }]"#,
            r#"routes: [{ match: { type: port, length: { min: 0, max: 4 } }, action: { pool: p } }]"#,
        ] {
            let yaml = format!(
                "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\nlisteners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    {bad}\n"
            );
            assert!(parse_str(&yaml).is_err(), "should reject: {bad}");
        }
    }

    /// A minimal TLS ClientHello record carrying `sni` in the SNI extension.
    fn client_hello(sni: &str) -> Vec<u8> {
        let mut sn = Vec::new();
        sn.extend_from_slice(&((sni.len() + 3) as u16).to_be_bytes()); // list len
        sn.push(0x00); // host_name
        sn.extend_from_slice(&(sni.len() as u16).to_be_bytes());
        sn.extend_from_slice(sni.as_bytes());

        let mut ext = Vec::new();
        ext.extend_from_slice(&0u16.to_be_bytes()); // server_name
        ext.extend_from_slice(&(sn.len() as u16).to_be_bytes());
        ext.extend_from_slice(&sn);

        let mut body = Vec::new();
        body.extend_from_slice(&[0x03, 0x03]); // client_version
        body.extend_from_slice(&[0u8; 32]); // random
        body.push(0x00); // session_id len
        body.extend_from_slice(&2u16.to_be_bytes()); // cipher_suites
        body.extend_from_slice(&[0x00, 0x2f]);
        body.push(0x01); // compression methods
        body.push(0x00);
        body.extend_from_slice(&(ext.len() as u16).to_be_bytes());
        body.extend_from_slice(&ext);

        let bl = body.len();
        let mut hs = vec![0x01, (bl >> 16) as u8, (bl >> 8) as u8, bl as u8];
        hs.extend_from_slice(&body);

        let mut rec = vec![0x16, 0x03, 0x01];
        rec.extend_from_slice(&(hs.len() as u16).to_be_bytes());
        rec.extend_from_slice(&hs);
        rec
    }

    #[test]
    fn extract_sni_reads_the_client_hello() {
        assert_eq!(
            extract_sni(&client_hello("EU.Example.COM")).as_deref(),
            Some("eu.example.com")
        );
        assert_eq!(extract_sni(b""), None);
        assert_eq!(extract_sni(b"\x16\x03\x01\x00\x05hello"), None); // truncated
        assert_eq!(extract_sni(&[0u8; 200]), None); // not a handshake
    }

    #[test]
    fn sni_matcher_exact_and_suffix() {
        let yaml = r#"
pools:
  - name: eu
    targets: ["127.0.0.1:1"]
  - name: lobby
    targets: ["127.0.0.1:2"]
listeners:
  - name: l
    bind: "0.0.0.0:443"
    routes:
      - match: { type: sni, host: ["*.eu.example.com", "special.example.net"] }
        action: { pool: eu }
      - match: { type: always }
        action: { pool: lobby }
"#;
        let cfg = parse_str(yaml).unwrap();
        let l = &cfg.listeners[0];
        assert_eq!(l.peek_len(), PEEK_MAX);
        let route = |sni: &str| {
            let ch = client_hello(sni);
            l.route_for(&ctx("9.9.9.9:5", "1.1.1.1:443", &ch))
                .map(str::to_string)
        };
        assert_eq!(route("a.eu.example.com").as_deref(), Some("eu"));
        assert_eq!(route("x.y.eu.example.com").as_deref(), Some("eu"));
        assert_eq!(route("special.example.net").as_deref(), Some("eu"));
        assert_eq!(route("eu.example.com").as_deref(), Some("lobby")); // suffix != apex
        assert_eq!(route("us.example.com").as_deref(), Some("lobby"));
        // no ClientHello at all -> sni route can't match
        assert_eq!(
            l.route_for(&ctx("9.9.9.9:5", "1.1.1.1:443", b"not tls")),
            Some("lobby")
        );
    }

    #[test]
    fn rejects_bad_sni_matchers() {
        for bad in [
            // sni on a udp listener
            "  - name: l\n    bind: \"0.0.0.0:443\"\n    protocol: udp\n    routes: [{ match: { type: sni, host: [\"a.example.com\"] }, action: { pool: p } }]",
            // empty host list
            "  - name: l\n    bind: \"0.0.0.0:443\"\n    routes: [{ match: { type: sni, host: [] }, action: { pool: p } }]",
            // '*' not as a leading label
            "  - name: l\n    bind: \"0.0.0.0:443\"\n    routes: [{ match: { type: sni, host: [\"a*b.example.com\"] }, action: { pool: p } }]",
            // wrong field
            "  - name: l\n    bind: \"0.0.0.0:443\"\n    routes: [{ match: { type: sni, cidrs: [\"10.0.0.0/8\"] }, action: { pool: p } }]",
        ] {
            let yaml = format!("pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\nlisteners:\n{bad}\n");
            assert!(parse_str(&yaml).is_err(), "should reject: {bad}");
        }
    }

    #[test]
    fn parses_sniffer_matcher_and_hints_the_listener() {
        let yaml = r#"
pools:
  - name: survival
    targets: ["127.0.0.1:1"]
  - name: lobby
    targets: ["127.0.0.1:2"]
listeners:
  - name: l
    bind: "0.0.0.0:25565"
    routes:
      - match: { type: sniffer, sniffer: minecraft, host: ["survival.example.net"] }
        action: { pool: survival }
      - match: { type: sniffer, sniffer: minecraft }
        action: { pool: lobby }
      - match: { type: always }
        action: { pool: lobby }
"#;
        let cfg = parse_str(yaml).unwrap();
        let l = &cfg.listeners[0];
        assert_eq!(l.sniffer.as_deref(), Some("minecraft"));
        assert_eq!(l.peek_len(), PEEK_MAX);

        let hint = |host: Option<&str>, reject: bool| RouteHint {
            host: host.map(str::to_string),
            key: None,
            reject,
        };
        // exact host -> survival
        assert_eq!(
            l.route_for(&ctx_sniff(&hint(Some("survival.example.net"), false))),
            Some("survival")
        );
        // recognised but different host -> the bare `sniffer` rule (lobby)
        assert_eq!(
            l.route_for(&ctx_sniff(&hint(Some("creative.example.net"), false))),
            Some("lobby")
        );
        // recognised, no host -> bare `sniffer` rule
        assert_eq!(l.route_for(&ctx_sniff(&hint(None, false))), Some("lobby"));
        // reject -> neither sniffer rule matches, falls to `always`
        assert_eq!(
            l.route_for(&ctx_sniff(&hint(Some("survival.example.net"), true))),
            Some("lobby")
        );
        // no hint at all -> falls to `always`
        assert_eq!(
            l.route_for(&ctx("9.9.9.9:5", "1.1.1.1:25565", &[])),
            Some("lobby")
        );
    }

    #[test]
    fn rejects_bad_sniffer_matchers() {
        for bad in [
            // missing sniffer name
            r#"routes: [{ match: { type: sniffer, host: ["a.example.com"] }, action: { pool: p } }]"#,
            // sniffer field on a non-sniffer matcher
            r#"routes: [{ match: { type: always, sniffer: sni }, action: { pool: p } }]"#,
            // two different sniffers on one listener
            r#"routes: [{ match: { type: sniffer, sniffer: minecraft }, action: { pool: p } }, { match: { type: sniffer, sniffer: a2s }, action: { pool: p } }]"#,
        ] {
            let yaml = format!(
                "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\nlisteners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    {bad}\n"
            );
            assert!(parse_str(&yaml).is_err(), "should reject: {bad}");
        }
    }

    #[test]
    fn parses_udp_prefix_listener() {
        let yaml = r#"
pools:
  - name: p
    targets: ["127.0.0.1:1"]
listeners:
  - name: l
    bind: "[::]:7777"
    protocol: udp
    prefix: "2001:db8:ace:1::/64"
    routes:
      - match: { type: dst, cidrs: ["2001:db8:ace:1::1/128"] }
        action: { pool: p }
      - match: { type: always }
        action: { pool: p }
"#;
        let cfg = parse_str(yaml).unwrap();
        let l = &cfg.listeners[0];
        let prefix = l.prefix.as_ref().unwrap();
        assert!(prefix.contains("2001:db8:ace:1::1".parse().unwrap()));
        assert!(!prefix.contains("2001:db8:ace:2::1".parse().unwrap()));
        assert!(!l.freebind);
    }

    #[test]
    fn rejects_bad_prefix_and_freebind() {
        for bad in [
            // prefix on a tcp listener
            "  - name: l\n    bind: \"[::]:7777\"\n    protocol: tcp\n    prefix: \"2001:db8::/64\"\n    pool: p",
            // prefix with a non-wildcard bind
            "  - name: l\n    bind: \"[2001:db8::1]:7777\"\n    protocol: udp\n    prefix: \"2001:db8::/64\"\n    pool: p",
            // unparseable prefix
            "  - name: l\n    bind: \"[::]:7777\"\n    protocol: udp\n    prefix: \"nonsense\"\n    pool: p",
            // freebind on a udp listener
            "  - name: l\n    bind: \"0.0.0.0:7777\"\n    protocol: udp\n    freebind: true\n    pool: p",
            // transparent + prefix together
            "  - name: l\n    bind: \"[::]:7777\"\n    protocol: udp\n    transparent: true\n    prefix: \"2001:db8::/64\"\n    pool: p",
        ] {
            let yaml = format!("pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\nlisteners:\n{bad}\n");
            assert!(parse_str(&yaml).is_err(), "should reject: {bad}");
        }
    }

    #[test]
    fn accepts_tcp_freebind_listener() {
        let yaml = r#"
pools:
  - name: p
    targets: ["127.0.0.1:1"]
listeners:
  - name: l
    bind: "198.51.100.7:443"
    freebind: true
    pool: p
"#;
        let cfg = parse_str(yaml).unwrap();
        assert!(cfg.listeners[0].freebind);
        assert!(cfg.listeners[0].prefix.is_none());
        assert!(!cfg.listeners[0].route_hint);
        assert!(!cfg.listeners[0].transparent);
    }

    #[test]
    fn parses_bind_port_range_listener() {
        let yaml = r#"
pools:
  - name: p
    targets: ["127.0.0.1:1"]
listeners:
  - name: l
    bind: "0.0.0.0:30000-30099"
    protocol: udp
    routes:
      - match: { type: port, ports: [30001] }
        action: { pool: p }
      - match: { type: always }
        action: { pool: p }
"#;
        let cfg = parse_str(yaml).unwrap();
        let l = &cfg.listeners[0];
        assert_eq!(l.bind, "0.0.0.0:30000".parse::<SocketAddr>().unwrap());
        assert_eq!(l.extra_binds.len(), 99);
        assert_eq!(
            l.extra_binds.last().copied().unwrap(),
            "0.0.0.0:30099".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(l.binds().count(), 100);
        assert!(l.binds().all(|a| a.ip() == std::net::Ipv4Addr::UNSPECIFIED));
    }

    #[test]
    fn parses_bind_port_range_ipv6() {
        let yaml = r#"
pools:
  - name: p
    targets: ["127.0.0.1:1"]
listeners:
  - name: l
    bind: "[2001:db8::1]:7000-7001"
    protocol: tcp
    pool: p
"#;
        let cfg = parse_str(yaml).unwrap();
        let l = &cfg.listeners[0];
        assert_eq!(l.bind, "[2001:db8::1]:7000".parse::<SocketAddr>().unwrap());
        assert_eq!(
            l.extra_binds,
            vec!["[2001:db8::1]:7001".parse::<SocketAddr>().unwrap()]
        );
    }

    #[test]
    fn bind_range_overlap_is_rejected_like_a_plain_bind() {
        // A range that overlaps another listener's single bind is rejected,
        // same as two plain binds colliding.
        let yaml = "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
                     listeners:\n  \
                     - name: a\n    bind: \"0.0.0.0:30050\"\n    pool: p\n  \
                     - name: b\n    bind: \"0.0.0.0:30000-30099\"\n    pool: p\n";
        assert!(parse_str(yaml).is_err());
    }

    #[test]
    fn rejects_bad_bind_ranges() {
        for bad in [
            // reversed range
            "0.0.0.0:30099-30000",
            // port 0 in range
            "0.0.0.0:0-10",
            // non-numeric bound
            "0.0.0.0:abc-30099",
            // way over the sanity cap
            "0.0.0.0:1-65000",
            // bad host
            "not-an-ip:30000-30099",
        ] {
            let yaml = format!(
                "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
                 listeners:\n  - name: l\n    bind: {bad:?}\n    pool: p\n"
            );
            assert!(parse_str(&yaml).is_err(), "should reject bind: {bad}");
        }
    }

    #[test]
    fn rejects_bind_range_combined_with_prefix() {
        let yaml = "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
                     listeners:\n  - name: l\n    bind: \"[::]:7777-7778\"\n    \
                     protocol: udp\n    prefix: \"2001:db8::/64\"\n    pool: p\n";
        assert!(parse_str(yaml).is_err());
    }

    #[test]
    fn parses_global_limits_and_rejects_zero() {
        let yaml = "settings:\n  limits:\n    max_connections: 5000\n    \
                    max_new_sessions_per_sec: 200\n\
                    pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
                    listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n";
        let lim = parse_str(yaml).unwrap().limits;
        assert_eq!(lim.max_connections, Some(5000));
        assert_eq!(lim.max_udp_sessions, None);
        assert_eq!(lim.max_new_sessions_per_sec, Some(200));

        let bad = "settings:\n  limits:\n    max_udp_sessions: 0\n\
                   pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
                   listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n";
        assert!(parse_str(bad).is_err());
    }

    #[test]
    fn absent_global_limits_are_empty() {
        let yaml = "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
                    listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n";
        assert!(parse_str(yaml).unwrap().limits.is_empty());
    }

    #[test]
    fn parses_geo_filter_and_uppercases_codes() {
        let yaml = r#"
settings:
  geo_db: "/tmp/whatever.mmdb"
pools:
  - name: p
    targets: ["127.0.0.1:1"]
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    pool: p
    geo:
      allow: ["se"]
      deny: ["Gb", "RU"]
"#;
        let cfg = parse_str(yaml).unwrap();
        assert_eq!(cfg.geo_db.as_deref(), Some("/tmp/whatever.mmdb"));
        let geo = cfg.listeners[0].geo.clone().unwrap();
        assert_eq!(geo.allow, vec![*b"SE"]);
        assert_eq!(geo.deny, vec![*b"GB", *b"RU"]);

        // deny wins; non-empty allow is default-deny; an unknown country is
        // admitted only with no allow list.
        assert!(geo.permits(Some(*b"SE")));
        assert!(!geo.permits(Some(*b"GB")));
        assert!(!geo.permits(Some(*b"FR"))); // not in allow
        assert!(!geo.permits(None)); // allow present -> fail closed
        let deny_only = GeoAcl {
            allow: vec![],
            deny: vec![*b"GB"],
        };
        assert!(deny_only.permits(None));
        assert!(deny_only.permits(Some(*b"FR")));
        assert!(!deny_only.permits(Some(*b"GB")));
    }

    #[test]
    fn rejects_bad_geo_filter() {
        for bad in [
            // geo without settings.geo_db
            "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
             listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n    \
             geo:\n      deny: [\"GB\"]",
            // empty geo
            "settings:\n  geo_db: \"/x.mmdb\"\n\
             pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
             listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n    \
             geo: {}",
            // not a 2-letter code
            "settings:\n  geo_db: \"/x.mmdb\"\n\
             pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
             listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n    \
             geo:\n      deny: [\"GBR\"]",
        ] {
            assert!(parse_str(bad).is_err(), "should reject: {bad}");
        }
    }

    #[test]
    fn parses_sniffers_settings_with_defaults_and_pins() {
        let yaml = "settings:\n  sniffers:\n    dir: \"/etc/gsp/sniffers\"\n\
                    pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
                    listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n";
        let s = parse_str(yaml).unwrap().sniffers.unwrap();
        assert_eq!(s.dir, "/etc/gsp/sniffers");
        assert_eq!(s.call_timeout, Duration::from_millis(20));
        assert_eq!(s.max_memory_bytes, 16 * 1024 * 1024);
        assert!(s.modules.is_empty());

        let yaml = format!(
            "settings:\n  sniffers:\n    dir: \"/plugins\"\n    call_timeout_ms: 5\n    \
             max_memory_bytes: 1048576\n    modules:\n      - name: a2s\n        sha256: \"{}\"\n      \
             - name: regex_firstbytes\n        sha256: \"{}\"\n        config: \"^GET \"\n\
             pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
             listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n",
            "AB".repeat(32),
            "CD".repeat(32),
        );
        let s = parse_str(&yaml).unwrap().sniffers.unwrap();
        assert_eq!(s.call_timeout, Duration::from_millis(5));
        assert_eq!(s.max_memory_bytes, 1_048_576);
        assert_eq!(s.modules.len(), 2);
        assert_eq!(s.modules[0].name, "a2s");
        assert_eq!(s.modules[0].sha256, "ab".repeat(32)); // lower-cased
        assert_eq!(s.modules[0].config, None);
        assert_eq!(s.modules[1].config.as_deref(), Some("^GET "));
    }

    #[test]
    fn absent_sniffers_settings_load_no_plugins() {
        let yaml = "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
                    listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n";
        assert!(parse_str(yaml).unwrap().sniffers.is_none());
    }

    #[test]
    fn rejects_bad_sniffers_settings() {
        for bad in [
            // empty dir
            "settings:\n  sniffers:\n    dir: \"\"\n\
             pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
             listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n",
            // zero call_timeout_ms
            "settings:\n  sniffers:\n    dir: \"/x\"\n    call_timeout_ms: 0\n\
             pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
             listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n",
            // zero max_memory_bytes
            "settings:\n  sniffers:\n    dir: \"/x\"\n    max_memory_bytes: 0\n\
             pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
             listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n",
            // empty module name
            "settings:\n  sniffers:\n    dir: \"/x\"\n    modules:\n      - name: \"\"\n        sha256: \"ab\"\n\
             pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
             listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n",
            // bad sha256 (too short, not hex)
            "settings:\n  sniffers:\n    dir: \"/x\"\n    modules:\n      - name: a2s\n        sha256: \"zz\"\n\
             pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
             listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n",
            // unknown field
            "settings:\n  sniffers:\n    dir: \"/x\"\n    bogus: 1\n\
             pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
             listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n",
        ] {
            assert!(parse_str(bad).is_err(), "should reject: {bad}");
        }

        // empty `config` string on a module
        let bad = format!(
            "settings:\n  sniffers:\n    dir: \"/x\"\n    modules:\n      - name: a2s\n        sha256: \"{}\"\n        config: \"\"\n\
             pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
             listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n",
            "ab".repeat(32),
        );
        assert!(parse_str(&bad).is_err(), "empty config must be rejected");
    }

    #[test]
    fn parses_per_source_cap_and_rejects_bad() {
        let yaml = "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
                    listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n    \
                    per_source:\n      max_per_ip: 50\n      max_per_net: 500\n";
        let ps = parse_str(yaml).unwrap().listeners[0].per_source.unwrap();
        assert_eq!(ps.max_per_ip, Some(50));
        assert_eq!(ps.max_per_net, Some(500));

        for bad in [
            // empty
            "    per_source: {}",
            // zero
            "    per_source:\n      max_per_ip: 0",
        ] {
            let y = format!(
                "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
                 listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n{bad}\n"
            );
            assert!(parse_str(&y).is_err(), "should reject: {bad}");
        }
    }

    #[test]
    fn first_packet_gate_parses_and_recognises_known_first_bytes() {
        let yaml = r#"
pools:
  - name: p
    targets: ["127.0.0.1:1"]
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    protocol: udp
    first_packet_gate: true
    routes:
      - match: { type: first_bytes, prefix: "hex:ffffffff" }
        action: { pool: p }
      - match: { type: always }
        action: { pool: p }
"#;
        let l = &parse_str(yaml).unwrap().listeners[0];
        assert!(l.first_packet_gate);
        let ctx = |b: &'static [u8]| MatchContext {
            src: "1.2.3.4:5".parse().unwrap(),
            local: "9.9.9.9:7777".parse().unwrap(),
            first_bytes: b,
            sniff: None,
        };
        assert!(l.first_packet_recognised(&ctx(&[0xff, 0xff, 0xff, 0xff, 0x01])));
        assert!(!l.first_packet_recognised(&ctx(b"random junk")));
    }

    #[test]
    fn rejects_bad_first_packet_gate() {
        for bad in [
            // tcp listener
            "  - name: l\n    bind: \"0.0.0.0:7777\"\n    protocol: tcp\n    first_packet_gate: true\n    pool: p",
            // udp but nothing to gate on
            "  - name: l\n    bind: \"0.0.0.0:7777\"\n    protocol: udp\n    first_packet_gate: true\n    pool: p",
        ] {
            let yaml = format!(
                "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\nlisteners:\n{bad}\n"
            );
            assert!(parse_str(&yaml).is_err(), "should reject: {bad}");
        }
    }

    #[test]
    fn parses_and_applies_listener_acl() {
        let yaml = r#"
pools:
  - name: p
    targets: ["127.0.0.1:1"]
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    pool: p
    allow: ["10.0.0.0/8", "192.168.0.0/16"]
    deny: ["10.6.6.0/24"]
"#;
        let acl = &parse_str(yaml).unwrap().listeners[0].acl;
        assert!(!acl.is_empty());
        assert!(acl.permits("10.1.2.3".parse().unwrap()));
        assert!(acl.permits("192.168.9.9".parse().unwrap()));
        // deny wins over an allow match
        assert!(!acl.permits("10.6.6.6".parse().unwrap()));
        // non-empty allow ⇒ default-deny for anything uncovered
        assert!(!acl.permits("8.8.8.8".parse().unwrap()));
    }

    #[test]
    fn deny_only_acl_is_default_allow() {
        let yaml = "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
                    listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n    \
                    deny: [\"203.0.113.0/24\"]\n";
        let acl = &parse_str(yaml).unwrap().listeners[0].acl;
        assert!(!acl.permits("203.0.113.5".parse().unwrap()));
        assert!(acl.permits("8.8.8.8".parse().unwrap()));
    }

    #[test]
    fn absent_acl_is_empty_and_permits_all() {
        let yaml = "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
                    listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n";
        let acl = &parse_str(yaml).unwrap().listeners[0].acl;
        assert!(acl.is_empty());
        assert!(acl.permits("8.8.8.8".parse().unwrap()));
    }

    #[test]
    fn rejects_bad_acl_cidr() {
        let yaml = "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
                    listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n    \
                    deny: [\"not-a-cidr\"]\n";
        assert!(parse_str(yaml).is_err());
    }

    #[test]
    fn parses_listener_rate_limit_with_burst_default() {
        let yaml = r#"
pools:
  - name: p
    targets: ["127.0.0.1:1"]
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    pool: p
    rate_limit:
      per_ip: { rate: 50 }
      per_net: { rate: 500, burst: 800 }
"#;
        let rl = parse_str(yaml).unwrap().listeners[0].rate_limit.unwrap();
        assert_eq!(
            rl.per_ip.unwrap(),
            TokenBucket {
                rate: 50,
                burst: 50
            }
        );
        assert_eq!(
            rl.per_net.unwrap(),
            TokenBucket {
                rate: 500,
                burst: 800
            }
        );
    }

    #[test]
    fn rejects_bad_rate_limit() {
        for bad in [
            // no bucket at all
            "    rate_limit: {}",
            // zero rate
            "    rate_limit:\n      per_ip: { rate: 0 }",
        ] {
            let yaml = format!(
                "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
                 listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n{bad}\n"
            );
            assert!(parse_str(&yaml).is_err(), "should reject: {bad}");
        }
    }

    #[test]
    fn absent_rate_limit_is_none() {
        let yaml = "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
                    listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n";
        assert!(parse_str(yaml).unwrap().listeners[0].rate_limit.is_none());
    }

    #[test]
    fn parses_tcp_transparent_listener() {
        let yaml = r#"
pools:
  - name: p
    targets: ["127.0.0.1:1"]
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    protocol: tcp
    transparent: true
    pool: p
"#;
        let cfg = parse_str(yaml).unwrap();
        assert!(cfg.listeners[0].transparent);
    }

    #[test]
    fn parses_udp_transparent_listener() {
        let cfg = parse_str(
            "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
             listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    protocol: udp\n    \
             transparent: true\n    pool: p\n",
        )
        .unwrap();
        assert!(cfg.listeners[0].transparent);
    }

    #[test]
    fn parses_route_hint_listener_flag() {
        let yaml = r#"
pools:
  - name: p
    targets: ["127.0.0.1:1"]
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    route_hint: true
    pool: p
"#;
        assert!(parse_str(yaml).unwrap().listeners[0].route_hint);
    }

    #[test]
    fn parses_resolver_and_resolver_route_action() {
        let yaml = r#"
pools:
  - name: lobby
    targets: ["127.0.0.1:1"]
resolvers:
  - name: matchmaker
    type: http
    endpoint: "https://mm.internal:8443/resolve"
    timeout_ms: 25
    on_error: fallback_route
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    routes:
      - match: { type: always }
        action: { resolver: matchmaker }
      - match: { type: always }
        action: { pool: lobby }
"#;
        let cfg = parse_str(yaml).unwrap();
        assert_eq!(cfg.resolvers.len(), 1);
        assert_eq!(cfg.resolvers[0].kind, ResolverKind::Http);
        assert_eq!(cfg.resolvers[0].timeout.as_millis(), 25);
        assert_eq!(cfg.resolvers[0].on_error, OnError::FallbackRoute);
        assert_eq!(
            cfg.listeners[0].routes[0].action,
            Action::Resolver("matchmaker".into())
        );
        // a resolver route forces a full peek so the resolver can see the bytes
        assert_eq!(cfg.listeners[0].peek_len(), PEEK_MAX);
        // default: no PROXY header for a target
        assert_eq!(cfg.resolvers[0].proxy_protocol, ProxyProtocol::None);
    }

    #[test]
    fn parses_resolver_target_proxy_protocol() {
        let yaml = r#"
pools: [{ name: p, targets: ["127.0.0.1:1"] }]
resolvers:
  - name: mm
    endpoint: "http://x"
    proxy_protocol: v2
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    routes:
      - { match: { type: always }, action: { resolver: mm } }
      - { match: { type: always }, action: { pool: p } }
"#;
        assert_eq!(
            parse_str(yaml).unwrap().resolvers[0].proxy_protocol,
            ProxyProtocol::V2
        );
    }

    #[test]
    fn rejects_v2_udp_resolver_on_a_tcp_listener() {
        let yaml = r#"
pools: [{ name: p, targets: ["127.0.0.1:1"] }]
resolvers:
  - name: mm
    endpoint: "http://x"
    proxy_protocol: v2-udp
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    routes:
      - { match: { type: always }, action: { resolver: mm } }
      - { match: { type: always }, action: { pool: p } }
"#;
        assert!(parse_str(yaml).is_err());
    }

    #[test]
    fn rejects_tcp_proxy_protocol_resolver_on_a_udp_listener() {
        let yaml = r#"
pools: [{ name: p, targets: ["127.0.0.1:1"] }]
resolvers:
  - name: mm
    endpoint: "http://x"
    proxy_protocol: v2
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    protocol: udp
    routes:
      - { match: { type: always }, action: { resolver: mm } }
      - { match: { type: always }, action: { pool: p } }
"#;
        assert!(parse_str(yaml).is_err());
    }

    #[test]
    fn parses_resolver_cache() {
        let yaml = r#"
pools: [{ name: p, targets: ["127.0.0.1:1"] }]
resolvers:
  - name: mm
    endpoint: "http://x"
    cache:
      key: ["src_ip", "sni", "first_bytes:0:16"]
      positive_ttl_sec: 60
      negative_ttl_sec: 5
      max_entries: 1000
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    pool: p
"#;
        let cfg = parse_str(yaml).unwrap();
        let c = cfg.resolvers[0].cache.as_ref().unwrap();
        assert_eq!(
            c.key,
            vec![
                CacheKeyPart::SrcIp,
                CacheKeyPart::Sni,
                CacheKeyPart::FirstBytes(0..=16)
            ]
        );
        assert_eq!(c.positive_ttl.as_secs(), 60);
        assert_eq!(c.negative_ttl.as_secs(), 5);
        assert_eq!(c.max_entries, 1000);
    }

    #[test]
    fn rejects_bad_resolver_cache() {
        for bad in [
            r#"cache: { key: [] }"#,
            r#"cache: { key: ["nonsense"] }"#,
            r#"cache: { key: ["first_bytes:8:4"] }"#,
            r#"cache: { key: ["first_bytes:0:99999"] }"#,
            r#"cache: { key: ["src_ip"], max_entries: 0 }"#,
        ] {
            let yaml = format!(
                "pools: [{{ name: p, targets: [\"127.0.0.1:1\"] }}]\nresolvers:\n  - {{ name: mm, endpoint: \"http://x\", {bad} }}\nlisteners:\n  - {{ name: l, bind: \"0.0.0.0:7777\", pool: p }}\n"
            );
            assert!(parse_str(&yaml).is_err(), "should reject: {bad}");
        }
    }

    #[test]
    fn rejects_bad_resolver_and_actions() {
        for bad in [
            // route references an undefined resolver
            "resolvers: []\nlisteners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    routes: [{ match: { type: always }, action: { resolver: nope } }]",
            // action with both pool and resolver
            "resolvers:\n  - { name: r, endpoint: \"http://x\" }\nlisteners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    routes: [{ match: { type: always }, action: { pool: p, resolver: r } }]",
            // action with neither
            "listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    routes: [{ match: { type: always }, action: {} }]",
            // unknown transport
            "resolvers:\n  - { name: r, type: smoke, endpoint: \"http://x\" }\nlisteners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p",
            // empty endpoint
            "resolvers:\n  - { name: r, endpoint: \"\" }\nlisteners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p",
        ] {
            let yaml = format!("pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n{bad}\n");
            assert!(parse_str(&yaml).is_err(), "should reject: {bad}");
        }
    }

    #[test]
    fn parses_resolver_target_timeouts_and_rejects_zero() {
        let with_resolver = |extra: &str| {
            format!(
                "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\nresolvers:\n  - {{ name: mm, endpoint: \"http://x\"{extra} }}\nlisteners:\n  - {{ name: l, bind: \"0.0.0.0:7777\", pool: p }}\n"
            )
        };

        // Defaults when omitted.
        let cfg = parse_str(&with_resolver("")).unwrap();
        assert_eq!(cfg.resolvers[0].target_connect_timeout.as_millis(), 300);
        assert_eq!(cfg.resolvers[0].target_idle_timeout.as_secs(), 90);

        // Explicit values parse.
        let cfg = parse_str(&with_resolver(
            ", target_connect_timeout_ms: 750, target_idle_timeout_sec: 20",
        ))
        .unwrap();
        assert_eq!(cfg.resolvers[0].target_connect_timeout.as_millis(), 750);
        assert_eq!(cfg.resolvers[0].target_idle_timeout.as_secs(), 20);

        // Zero for either is rejected.
        for bad in [
            ", target_connect_timeout_ms: 0",
            ", target_idle_timeout_sec: 0",
        ] {
            assert!(
                parse_str(&with_resolver(bad)).is_err(),
                "should reject: {bad}"
            );
        }
    }

    #[test]
    fn rejects_pool_and_routes_together() {
        let yaml = r#"
pools:
  - name: p
    targets: ["127.0.0.1:1"]
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    pool: p
    routes:
      - match: { type: always }
        action: { pool: p }
"#;
        assert!(parse_str(yaml).is_err());
    }

    #[test]
    fn rejects_route_to_unknown_pool() {
        let yaml = r#"
pools:
  - name: p
    targets: ["127.0.0.1:1"]
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    routes:
      - match: { type: always }
        action: { pool: nope }
"#;
        assert!(parse_str(yaml).is_err());
    }

    #[test]
    fn rejects_always_matcher_with_fields() {
        let yaml = r#"
pools:
  - name: p
    targets: ["127.0.0.1:1"]
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    routes:
      - match: { type: always, cidrs: ["10.0.0.0/8"] }
        action: { pool: p }
"#;
        assert!(parse_str(yaml).is_err());
    }

    #[test]
    fn rejects_bad_cidr_and_reversed_port_range() {
        for bad in [
            r#"routes: [{ match: { type: client_cidr, cidrs: ["10.0.0.0/33"] }, action: { pool: p } }]"#,
            r#"routes: [{ match: { type: port, ports: ["9000-8000"] }, action: { pool: p } }]"#,
            r#"routes: [{ match: { type: client_cidr }, action: { pool: p } }]"#,
        ] {
            let yaml = format!(
                "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\nlisteners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    {bad}\n"
            );
            assert!(parse_str(&yaml).is_err(), "should reject: {bad}");
        }
    }

    #[test]
    fn cidr_contains_v4_and_v6() {
        let c = Cidr::parse("10.0.0.0/8").unwrap();
        assert!(c.contains("10.255.1.1".parse().unwrap()));
        assert!(!c.contains("11.0.0.1".parse().unwrap()));
        let c6 = Cidr::parse("2001:db8::/32").unwrap();
        assert!(c6.contains("2001:db8:dead:beef::1".parse().unwrap()));
        assert!(!c6.contains("2001:db9::1".parse().unwrap()));
        assert!(!c.contains("2001:db8::1".parse().unwrap()));
    }

    #[test]
    fn cidr_set_membership_v4_v6_and_prefix_subsumption() {
        let cidrs: Vec<Cidr> = ["10.0.0.0/8", "192.168.1.0/24", "2001:db8::/32"]
            .iter()
            .map(|s| Cidr::parse(s).unwrap())
            .collect();
        let set = CidrSet::build(&cidrs);
        assert_eq!(set.len(), 3);
        assert!(set.contains("10.9.9.9".parse().unwrap()));
        assert!(set.contains("192.168.1.200".parse().unwrap()));
        assert!(!set.contains("192.168.2.1".parse().unwrap()));
        assert!(!set.contains("11.0.0.1".parse().unwrap()));
        assert!(set.contains("2001:db8:dead::1".parse().unwrap()));
        assert!(!set.contains("2001:db9::1".parse().unwrap()));

        // A shorter prefix already in the set makes a longer one redundant, and
        // the reverse insertion order still yields the covering answer.
        let s2 = CidrSet::build(
            &["10.1.2.0/24", "10.0.0.0/8"]
                .iter()
                .map(|s| Cidr::parse(s).unwrap())
                .collect::<Vec<_>>(),
        );
        assert!(s2.contains("10.1.2.3".parse().unwrap()));
        assert!(s2.contains("10.240.0.1".parse().unwrap()));

        // The empty set matches nothing; a /0 matches everything.
        assert!(!CidrSet::default().contains("8.8.8.8".parse().unwrap()));
        let all_v4 = CidrSet::build(&[Cidr::parse("0.0.0.0/0").unwrap()]);
        assert!(all_v4.contains("1.2.3.4".parse().unwrap()));
        assert!(!all_v4.contains("::1".parse().unwrap()));
    }

    // -----------------------------------------------------------------------
    // backend_sources (phase 8)
    // -----------------------------------------------------------------------

    #[test]
    fn static_source_folds_into_pool_targets() {
        let yaml = r#"
backend_sources:
  - name: eu
    type: static
    targets: ["10.1.0.1:7777", "10.1.0.2:7777"]
pools:
  - { name: p, source: eu }
listeners:
  - { name: l, bind: "0.0.0.0:7777", pool: p }
"#;
        let cfg = parse_str(yaml).unwrap();
        assert_eq!(
            cfg.pools[0].targets,
            vec![
                "10.1.0.1:7777".parse().unwrap(),
                "10.1.0.2:7777".parse().unwrap()
            ]
        );
        assert!(cfg.pools[0].source.is_none());
    }

    #[test]
    fn dynamic_source_attaches_to_the_pool_with_an_empty_seed() {
        let yaml = r#"
backend_sources:
  - name: us
    type: dns_srv
    record: "_game._udp.us.example.com"
    refresh_interval_sec: 10
pools:
  - { name: p, source: us }
listeners:
  - { name: l, bind: "0.0.0.0:7777", pool: p }
"#;
        let cfg = parse_str(yaml).unwrap();
        assert!(cfg.pools[0].targets.is_empty());
        let sc = cfg.pools[0].source.as_ref().unwrap();
        assert_eq!(sc.name, "us");
        assert_eq!(sc.refresh_interval, Duration::from_secs(10));
        assert!(matches!(
            &sc.kind,
            SourceKind::DnsSrv { record } if record == "_game._udp.us.example.com"
        ));
    }

    #[test]
    fn consul_and_kubernetes_sources_apply_defaults() {
        let yaml = r#"
backend_sources:
  - { name: c, type: consul, service: game }
  - { name: k, type: kubernetes, service: match }
pools:
  - { name: pc, source: c }
  - { name: pk, source: k }
listeners:
  - { name: l, bind: "0.0.0.0:7777", pool: pc }
"#;
        let cfg = parse_str(yaml).unwrap();
        let by = |n: &str| {
            cfg.pools
                .iter()
                .find(|p| p.name == n)
                .unwrap()
                .source
                .clone()
                .unwrap()
        };
        assert!(matches!(
            by("pc").kind,
            SourceKind::Consul { addr, tag: None, .. } if addr == "http://127.0.0.1:8500"
        ));
        assert!(matches!(
            by("pk").kind,
            SourceKind::Kubernetes { namespace, api, port_name: None, .. }
                if namespace == "default" && api == "https://kubernetes.default.svc"
        ));
    }

    #[test]
    fn rejects_targets_and_source_together() {
        let yaml = r#"
backend_sources: [{ name: s, type: static, targets: ["10.0.0.1:1"] }]
pools:
  - { name: p, targets: ["127.0.0.1:1"], source: s }
listeners:
  - { name: l, bind: "0.0.0.0:7777", pool: p }
"#;
        assert!(parse_str(yaml).is_err());
    }

    #[test]
    fn rejects_pool_with_neither_targets_nor_source() {
        let yaml = r#"
pools:
  - { name: p }
listeners:
  - { name: l, bind: "0.0.0.0:7777", pool: p }
"#;
        assert!(parse_str(yaml).is_err());
    }

    #[test]
    fn rejects_unknown_source_reference_and_bad_source_specs() {
        // unknown reference
        assert!(parse_str(
            r#"
pools: [{ name: p, source: nope }]
listeners: [{ name: l, bind: "0.0.0.0:7777", pool: p }]
"#
        )
        .is_err());
        // dns_srv without `record`
        assert!(parse_str(
            r#"
backend_sources: [{ name: s, type: dns_srv }]
pools: [{ name: p, source: s }]
listeners: [{ name: l, bind: "0.0.0.0:7777", pool: p }]
"#
        )
        .is_err());
        // refresh_interval_sec: 0
        assert!(parse_str(
            r#"
backend_sources: [{ name: s, type: consul, service: g, refresh_interval_sec: 0 }]
pools: [{ name: p, source: s }]
listeners: [{ name: l, bind: "0.0.0.0:7777", pool: p }]
"#
        )
        .is_err());
        // unknown type
        assert!(parse_str(
            r#"
backend_sources: [{ name: s, type: etcd, service: g }]
pools: [{ name: p, source: s }]
listeners: [{ name: l, bind: "0.0.0.0:7777", pool: p }]
"#
        )
        .is_err());
        // tunnel without `pubkey`
        assert!(parse_str(
            r#"
backend_sources: [{ name: s, type: tunnel }]
pools: [{ name: p, source: s }]
listeners: [{ name: l, bind: "0.0.0.0:7777", pool: p }]
"#
        )
        .is_err());
        // tunnel with a malformed pubkey (not 32 bytes of base64)
        assert!(parse_str(
            r#"
backend_sources: [{ name: s, type: tunnel, pubkey: "not-a-key" }]
pools: [{ name: p, source: s }]
listeners: [{ name: l, bind: "0.0.0.0:7777", pool: p }]
"#
        )
        .is_err());
    }

    #[test]
    fn tunnel_source_attaches_to_the_pool_with_the_pinned_pubkey() {
        let yaml = r#"
backend_sources:
  - name: home
    type: tunnel
    pubkey: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
    refresh_interval_sec: 10
pools:
  - { name: p, source: home }
listeners:
  - { name: l, bind: "0.0.0.0:7777", pool: p }
"#;
        let cfg = parse_str(yaml).unwrap();
        assert!(cfg.pools[0].targets.is_empty());
        let sc = cfg.pools[0].source.as_ref().unwrap();
        assert_eq!(sc.name, "home");
        assert!(matches!(
            &sc.kind,
            SourceKind::Tunnel { pubkey }
                if pubkey == "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
        ));
    }

    fn gossip_fixture(settings_extra: &str) -> String {
        format!(
            "settings:\n{settings_extra}\
             pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
             listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n"
        )
    }

    #[test]
    fn absent_gossip_is_none() {
        let yaml = "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
                    listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n";
        let cfg = parse_str(yaml).unwrap();
        assert!(cfg.failure_domain.is_none());
        assert!(cfg.gossip.is_none());
    }

    #[test]
    fn parses_gossip_settings_with_defaults() {
        let yaml = gossip_fixture(
            "  failure_domain: \"eu-west-1a\"\n\
             \x20 gossip:\n    bind: \"0.0.0.0:7946\"\n    psk: \"secret\"\n",
        );
        let cfg = parse_str(&yaml).unwrap();
        assert_eq!(cfg.failure_domain.as_deref(), Some("eu-west-1a"));
        let g = cfg.gossip.unwrap();
        assert_eq!(g.bind, "0.0.0.0:7946".parse().unwrap());
        assert!(g.seeds.is_empty());
        assert_eq!(g.quorum_fraction, 0.66);
        assert_eq!(g.psk, "secret");
    }

    #[test]
    fn parses_gossip_seeds_and_quorum_fraction() {
        let yaml = gossip_fixture(
            "  failure_domain: \"eu-west-1a\"\n\
             \x20 gossip:\n    bind: \"0.0.0.0:7946\"\n    psk: \"secret\"\n    \
             quorum_fraction: 0.75\n    seeds: [\"10.0.0.1:7946\", \"10.0.0.2:7946\"]\n",
        );
        let g = parse_str(&yaml).unwrap().gossip.unwrap();
        assert_eq!(g.quorum_fraction, 0.75);
        assert_eq!(
            g.seeds,
            vec![
                "10.0.0.1:7946".parse().unwrap(),
                "10.0.0.2:7946".parse().unwrap(),
            ]
        );
    }

    #[test]
    fn rejects_gossip_without_failure_domain_and_vice_versa() {
        let gossip_only =
            gossip_fixture("  gossip:\n    bind: \"0.0.0.0:7946\"\n    psk: \"secret\"\n");
        assert!(parse_str(&gossip_only).is_err());

        let domain_only = gossip_fixture("  failure_domain: \"eu-west-1a\"\n");
        assert!(parse_str(&domain_only).is_err());
    }

    #[test]
    fn rejects_bad_gossip_settings() {
        for bad in [
            // bad bind address
            gossip_fixture(
                "  failure_domain: \"d\"\n  gossip:\n    bind: \"nope\"\n    psk: \"secret\"\n",
            ),
            // bad seed address
            gossip_fixture(
                "  failure_domain: \"d\"\n  gossip:\n    bind: \"0.0.0.0:7946\"\n    \
                 psk: \"secret\"\n    seeds: [\"nope\"]\n",
            ),
            // empty psk
            gossip_fixture(
                "  failure_domain: \"d\"\n  gossip:\n    bind: \"0.0.0.0:7946\"\n    psk: \"\"\n",
            ),
            // quorum_fraction too low
            gossip_fixture(
                "  failure_domain: \"d\"\n  gossip:\n    bind: \"0.0.0.0:7946\"\n    \
                 psk: \"secret\"\n    quorum_fraction: 0.5\n",
            ),
            // quorum_fraction too high
            gossip_fixture(
                "  failure_domain: \"d\"\n  gossip:\n    bind: \"0.0.0.0:7946\"\n    \
                 psk: \"secret\"\n    quorum_fraction: 1.5\n",
            ),
        ] {
            assert!(parse_str(&bad).is_err(), "should reject: {bad}");
        }
    }

    #[test]
    fn absent_group_is_none() {
        let yaml = "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
                    listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n";
        assert!(parse_str(yaml).unwrap().group.is_none());
    }

    #[test]
    fn parses_valid_multi_segment_group() {
        let yaml = gossip_fixture("  group: \"eu/frankfurt/cluster-a\"\n");
        assert_eq!(
            parse_str(&yaml).unwrap().group.as_deref(),
            Some("eu/frankfurt/cluster-a")
        );
    }

    #[test]
    fn rejects_bad_group_paths() {
        for bad in [
            gossip_fixture("  group: \"\"\n"),
            gossip_fixture("  group: \"/eu/frankfurt\"\n"),
            gossip_fixture("  group: \"eu/frankfurt/\"\n"),
            gossip_fixture("  group: \"eu//frankfurt\"\n"),
        ] {
            assert!(parse_str(&bad).is_err(), "should reject: {bad}");
        }
    }
}
