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
    pools: Vec<RawPool>,
    #[serde(default)]
    listeners: Vec<RawListener>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawSettings {
    /// Accept-loop tasks per listener. `0` means "one per CPU core".
    #[serde(default)]
    workers: usize,
    #[serde(default)]
    admin: RawAdmin,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAdmin {
    #[serde(default = "default_admin_listen")]
    listen: String,
}

impl Default for RawAdmin {
    fn default() -> Self {
        Self {
            listen: default_admin_listen(),
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
    targets: Vec<String>,
    #[serde(default)]
    balancer: Balancer,
    /// `consistent_hash` only: which part of the client address is the hash key.
    #[serde(default)]
    hash_on: Option<HashOn>,
    #[serde(default = "default_connect_timeout_ms")]
    connect_timeout_ms: u64,
    #[serde(default = "default_idle_timeout_sec")]
    idle_timeout_sec: u64,
    #[serde(default)]
    health_check: RawHealthCheck,
    #[serde(default)]
    per_backend: RawPerBackend,
}

fn default_connect_timeout_ms() -> u64 {
    300
}

fn default_idle_timeout_sec() -> u64 {
    90
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
    /// Consult the push-resolver table (`POST /route-hint`) before the route
    /// list: a live `src_ip → pool` hint wins if its pool still exists.
    #[serde(default)]
    route_hint: bool,
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAction {
    pool: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAffinity {
    #[serde(default)]
    hash_on: HashOn,
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
    /// The sniffer wants this connection rejected outright.
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
    /// of them; empty `host` ⇒ matches on any (non-`reject`) recognition. The
    /// name is not validated here — the proxy checks it against the loaded
    /// sniffers at listener start (an unknown name simply never matches).
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
/// SNI. A ClientHello fragmented across TCP segments (so only part is in the
/// peek buffer) yields `None` — that route then just does not match.
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

/// One rule in a listener's ordered route list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub matcher: Matcher,
    pub pool: String,
}

// ---------------------------------------------------------------------------
// Validated types: parsed, resolved, ready for the runtime to consume.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Config {
    /// `0` means "one worker per CPU core".
    pub workers: usize,
    pub admin_listen: SocketAddr,
    pub pools: Vec<PoolConfig>,
    pub listeners: Vec<ListenerConfig>,
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

#[derive(Debug, Clone)]
pub struct PoolConfig {
    pub name: String,
    pub targets: Vec<SocketAddr>,
    pub balancer: Balancer,
    /// `Some` iff `balancer == ConsistentHash`: the hash key selector.
    pub hash_on: Option<HashOn>,
    pub connect_timeout: Duration,
    pub idle_timeout: Duration,
    pub health_check: HealthCheck,
    /// Max concurrent sessions per backend, if capped.
    pub max_sessions: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListenerConfig {
    pub name: String,
    pub bind: SocketAddr,
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
    /// The single sniffer plugin this listener's routes use (`None` if no
    /// `sniffer` route). `gsp-core` runs it once per connection before routing.
    pub sniffer: Option<String>,
    /// Check the `POST /route-hint` push-resolver table before the route list.
    pub route_hint: bool,
}

impl ListenerConfig {
    /// The pool name for a connection/session described by `ctx`, or `None` when
    /// no route matches.
    pub fn route_for(&self, ctx: &MatchContext) -> Option<&str> {
        self.routes
            .iter()
            .find(|r| r.matcher.matches(ctx))
            .map(|r| r.pool.as_str())
    }

    /// How many leading bytes to `MSG_PEEK` before routing (0 = no byte
    /// matcher, skip the peek entirely).
    pub fn peek_len(&self) -> usize {
        self.routes
            .iter()
            .map(|r| r.matcher.peek_len())
            .max()
            .unwrap_or(0)
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

    let mut pool_names = BTreeSet::new();
    let mut pools = Vec::with_capacity(raw.pools.len());
    for p in raw.pools {
        if !pool_names.insert(p.name.clone()) {
            return Err(Invalid(format!("duplicate pool name: {}", p.name)));
        }
        if p.targets.is_empty() {
            return Err(Invalid(format!("pool {} has no targets", p.name)));
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

        pools.push(PoolConfig {
            name: p.name,
            targets,
            balancer: p.balancer,
            hash_on,
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
        });
    }

    let mut listener_names = BTreeSet::new();
    let mut binds = BTreeSet::new();
    let mut listeners = Vec::with_capacity(raw.listeners.len());
    for l in raw.listeners {
        if !listener_names.insert(l.name.clone()) {
            return Err(Invalid(format!("duplicate listener name: {}", l.name)));
        }
        let bind: SocketAddr = l.bind.parse().map_err(|_| {
            Invalid(format!(
                "listener {}: bind is not a valid socket address: {}",
                l.name, l.bind
            ))
        })?;
        if !binds.insert((bind, l.protocol)) {
            return Err(Invalid(format!(
                "listener {}: bind {bind} is already used by another listener",
                l.name
            )));
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
                if !pool_names.contains(&r.action.pool) {
                    return Err(Invalid(format!(
                        "listener {}: route {i}: unknown pool {}",
                        l.name, r.action.pool
                    )));
                }
                rs.push(Route {
                    matcher,
                    pool: r.action.pool.clone(),
                });
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
                pool,
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
            protocol: l.protocol,
            routes,
            affinity,
            prefix,
            freebind: l.freebind,
            sniffer,
            route_hint: l.route_hint,
        });
    }

    Ok(Config {
        workers: raw.settings.workers,
        admin_listen,
        pools,
        listeners,
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
                .map(|h| parse_host_pattern(h).map_err(&at))
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
                cidrs.push(Cidr::parse(c).map_err(&at)?);
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
                Some(s) => parse_byte_spec(s).map_err(&at)?,
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
        assert_eq!(cfg.pools[0].connect_timeout.as_millis(), 300);
        assert_eq!(cfg.pools[0].balancer, Balancer::RoundRobin);
        assert_eq!(cfg.pools[0].health_check.rise, 2);
        assert_eq!(cfg.pools[0].health_check.fall, 3);
        assert!(cfg.pools[0].max_sessions.is_none());
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
        assert_eq!(cfg.listeners[0].routes[0].pool, "local");
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
}
