//! Configuration types, parsing and validation for the game server proxy.
//!
//! This is the reduced **v0 schema**: TCP/UDP listeners with a priority-ordered
//! route rule list (`always` / `client_cidr` / `dst` / `port` / `first_bytes` /
//! `sni` matchers; `sniffer` for future plugins) onto static pools, with active
//! health checks, round-robin /
//! least-connections / consistent-hash balancing (UDP session affinity), and per-backend
//! session caps. The full target schema lives in
//! `docs/05-configuration.md` and grows into this crate incrementally.

use std::path::Path;

mod cidr;
mod keys;
mod matcher;
mod parse;
mod resolved;
mod schema;
mod validate;

pub use cidr::{Acl, Cidr, CidrSet, GeoAcl};
pub use keys::base64_decode_32;
pub use matcher::{extract_sni, HostPattern, MatchContext, Matcher, RouteHint, PEEK_MAX};
pub use resolved::{
    Action, Balancer, CacheConfig, CacheKeyPart, Config, GlobalLimits, GossipConfig, HashOn,
    HealthCheck, HealthCheckKind, ListenerConfig, OnError, PerSourceLimit, PoolConfig, Protocol,
    ProxyProtocol, RateLimit, ResolverConfig, ResolverKind, Route, SnifferModulePin,
    SniffersConfig, SourceConfig, SourceKind, TokenBucket,
};
pub use schema::AdminTls;

use schema::RawConfig;
use validate::validate;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse YAML: {0}")]
    Parse(#[from] serde_norway::Error),
    #[error("invalid configuration: {0}")]
    Invalid(String),
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
    let raw: RawConfig = serde_norway::from_str(text)?;
    validate(raw)
}
