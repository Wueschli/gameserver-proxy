//! Configuration types, parsing and validation for the game server proxy.
//!
//! This is the reduced **v0 schema** used by the walking skeleton: TCP
//! listeners forwarding to static pools, with active health checks, a
//! round-robin / least-connections balancer, and per-backend session caps.
//! The full target schema lives in `docs/05-configuration.md` and grows into
//! this crate incrementally.

use std::collections::BTreeSet;
use std::net::SocketAddr;
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
    pool: String,
    /// UDP only: per-client → backend stickiness across session re-creation.
    #[serde(default)]
    affinity: Option<RawAffinity>,
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
    pub pool: String,
    /// `Some` on UDP listeners (stickiness key); `None` on TCP.
    pub affinity: Option<HashOn>,
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
        if !pool_names.contains(&l.pool) {
            return Err(Invalid(format!(
                "listener {}: unknown pool {}",
                l.name, l.pool
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
        listeners.push(ListenerConfig {
            name: l.name,
            bind,
            protocol: l.protocol,
            pool: l.pool,
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
}
