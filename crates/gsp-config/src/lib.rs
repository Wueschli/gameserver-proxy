//! Configuration types, parsing and validation for the game server proxy.
//!
//! This is the reduced **v0 schema**: TCP/UDP listeners with a priority-ordered
//! route rule list (`always` / `client_cidr` / `port` / `first_bytes` matchers)
//! onto static pools, with active health checks, round-robin / least-connections
//! balancing, per-backend session caps, and UDP session affinity. The full
//! target schema lives in `docs/05-configuration.md` and grows into this crate
//! incrementally.

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
    /// `client_cidr` only: source-IP prefixes.
    #[serde(default)]
    cidrs: Option<Vec<String>>,
    /// `port` only: destination ports (bare int `30001` or `"lo-hi"` range).
    #[serde(default)]
    ports: Option<Vec<RawPort>>,
    /// `first_bytes` only: `"hex:ffffffff"` or `"ascii:hello"`.
    #[serde(default)]
    prefix: Option<String>,
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

/// A single route's match condition. Address-based only for now; `first_bytes`,
/// `sni`, sniffer and `external` matchers arrive later in phase 3–4.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Matcher {
    /// Catch-all.
    Always,
    /// Source IP within any of these prefixes.
    ClientCidr(Vec<Cidr>),
    /// Destination port (from the accepting socket) within any of these ranges.
    DstPort(Vec<RangeInclusive<u16>>),
    /// The connection's first bytes (TCP peek / first UDP datagram) start with
    /// this non-empty prefix. `regex` / `length` / `sniffer` variants come later.
    FirstBytes(Vec<u8>),
}

/// Hard cap on how many leading bytes a `first_bytes` prefix may match, and thus
/// how far the TCP path will `MSG_PEEK`.
pub const PEEK_MAX: usize = 512;

/// Everything a [`Matcher`] can look at. `first_bytes` is empty when the listener
/// has no byte matcher (so nothing was peeked) or the peer sent nothing yet.
pub struct MatchContext<'a> {
    pub src: SocketAddr,
    pub local: SocketAddr,
    pub first_bytes: &'a [u8],
}

impl Matcher {
    pub fn matches(&self, ctx: &MatchContext) -> bool {
        match self {
            Matcher::Always => true,
            Matcher::ClientCidr(cidrs) => cidrs.iter().any(|c| c.contains(ctx.src.ip())),
            Matcher::DstPort(ranges) => ranges.iter().any(|r| r.contains(&ctx.local.port())),
            Matcher::FirstBytes(prefix) => ctx.first_bytes.starts_with(prefix),
        }
    }

    /// Leading bytes this matcher needs to see (0 for address-only matchers).
    fn peek_len(&self) -> usize {
        match self {
            Matcher::FirstBytes(prefix) => prefix.len(),
            _ => 0,
        }
    }
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

        pools.push(PoolConfig {
            name: p.name,
            targets,
            balancer: p.balancer,
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
        listeners.push(ListenerConfig {
            name: l.name,
            bind,
            protocol: l.protocol,
            routes,
            affinity,
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
    allow("cidrs", m.cidrs.is_some(), m.kind == "client_cidr")?;
    allow("ports", m.ports.is_some(), m.kind == "port")?;
    allow("prefix", m.prefix.is_some(), m.kind == "first_bytes")?;

    match m.kind.as_str() {
        "always" => Ok(Matcher::Always),
        "client_cidr" => {
            let raw = m.cidrs.as_ref().filter(|c| !c.is_empty()).ok_or_else(|| {
                at("match type `client_cidr` needs a non-empty `cidrs` list".into())
            })?;
            let mut cidrs = Vec::with_capacity(raw.len());
            for c in raw {
                cidrs.push(Cidr::parse(c).map_err(&at)?);
            }
            Ok(Matcher::ClientCidr(cidrs))
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
            let spec = m
                .prefix
                .as_ref()
                .ok_or_else(|| at("match type `first_bytes` needs a `prefix`".into()))?;
            let bytes = parse_byte_spec(spec).map_err(&at)?;
            if bytes.is_empty() {
                return Err(at("`first_bytes` prefix must not be empty".into()));
            }
            if bytes.len() > PEEK_MAX {
                return Err(at(format!(
                    "`first_bytes` prefix is {} bytes, over the {PEEK_MAX}-byte limit",
                    bytes.len()
                )));
            }
            Ok(Matcher::FirstBytes(bytes))
        }
        other => Err(at(format!(
            "unknown match type {other:?} (always | client_cidr | port | first_bytes)"
        ))),
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
    fn rejects_bad_first_bytes_specs() {
        for bad in [
            r#"routes: [{ match: { type: first_bytes, prefix: "ffff" }, action: { pool: p } }]"#,
            r#"routes: [{ match: { type: first_bytes }, action: { pool: p } }]"#,
            r#"routes: [{ match: { type: first_bytes, prefix: "hex:" }, action: { pool: p } }]"#,
            r#"routes: [{ match: { type: always, prefix: "hex:ff" }, action: { pool: p } }]"#,
        ] {
            let yaml = format!(
                "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\nlisteners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    {bad}\n"
            );
            assert!(parse_str(&yaml).is_err(), "should reject: {bad}");
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
}
