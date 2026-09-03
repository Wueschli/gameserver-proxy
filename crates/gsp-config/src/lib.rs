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
}

impl Default for RawHealthCheck {
    fn default() -> Self {
        Self {
            kind: default_hc_type(),
            interval_sec: default_hc_interval_sec(),
            timeout_ms: default_hc_timeout_ms(),
            rise: default_hc_rise(),
            fall: default_hc_fall(),
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
        if hc.kind != "tcp_connect" {
            return Err(Invalid(format!(
                "pool {}: health_check.type {:?} is not supported (only tcp_connect)",
                p.name, hc.kind
            )));
        }
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
        if l.protocol == Protocol::Udp {
            return Err(Invalid(format!(
                "listener {}: UDP is not implemented yet (scaffold supports TCP only)",
                l.name
            )));
        }
        listeners.push(ListenerConfig {
            name: l.name,
            bind,
            protocol: l.protocol,
            pool: l.pool,
        });
    }

    Ok(Config {
        workers: raw.settings.workers,
        admin_listen,
        pools,
        listeners,
    })
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
    fn rejects_udp_for_now() {
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
