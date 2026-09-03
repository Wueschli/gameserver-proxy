//! Configuration types, parsing and validation for the game server proxy.
//!
//! This is the reduced **v0 schema** used by the walking skeleton: one TCP
//! listener forwarding to a static pool. The full target schema lives in
//! `docs/05-configuration.md` and will grow into this crate incrementally.

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
    /// Worker tasks per listener. `0` means "one per CPU core".
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
}

fn default_connect_timeout_ms() -> u64 {
    300
}

fn default_idle_timeout_sec() -> u64 {
    90
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
pub struct PoolConfig {
    pub name: String,
    pub targets: Vec<SocketAddr>,
    pub balancer: Balancer,
    pub connect_timeout: Duration,
    pub idle_timeout: Duration,
}

#[derive(Debug, Clone)]
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
        pools.push(PoolConfig {
            name: p.name,
            targets,
            balancer: p.balancer,
            connect_timeout: Duration::from_millis(p.connect_timeout_ms),
            idle_timeout: Duration::from_secs(p.idle_timeout_sec),
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
