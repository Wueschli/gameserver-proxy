//! The `tunnel` backend source (phase 14 slice 5, `docs/11`), behind the `tunnel`
//! cargo feature; `tunnel_source_disabled.rs` stands in without it.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context};
use async_trait::async_trait;
use serde::Deserialize;
use wayhouse_core::{BackendSource, SourceError};

/// Which `wayhouse-controller` backend-peers registry a `tunnel` source resolves
/// against (phase 14 slice 5, `docs/11`) — the identical registry `wayhouse`'s
/// own `--tunnel-iface` reconcile task (`tunnel_client.rs`) subscribes to,
/// so this is built from the same `--tunnel-controller-url`/
/// `--tunnel-controller-token` flags rather than a second pair.
#[derive(Debug, Clone)]
pub struct TunnelRegistry {
    pub controller_url: String,
    pub token: Option<String>,
}

// ---------------------------------------------------------------------------
// Tunnel (phase 14 backend transport, docs/11) — resolves an origin's
// currently-registered backends from wayhouse-controller's backend-peers
// registry (phase 14 slice 2), the same registry wayhouse's own `--tunnel-iface`
// reconcile task subscribes to (`tunnel_client.rs`).
// ---------------------------------------------------------------------------

/// The `tunnel` arm of `discovery::build_one`.
pub fn build(
    pool: String,
    source: &str,
    pubkey: &str,
    registry: Option<&TunnelRegistry>,
    interval: Duration,
) -> anyhow::Result<Arc<dyn BackendSource>> {
    let registry = registry.ok_or_else(|| {
        anyhow!(
            "pool {pool}: backend_sources {source} is type tunnel but \
             --tunnel-controller-url is not set"
        )
    })?;
    Ok(Arc::new(TunnelSource::new(
        pool,
        source.to_string(),
        pubkey.to_string(),
        registry.controller_url.clone(),
        registry.token.clone(),
        interval,
    )?))
}

pub struct TunnelSource {
    pool: String,
    /// The `backend_sources[].name` — also this origin's registered name in
    /// the peers registry (`GET /peers/{origin}`); the two are the same
    /// identifier by construction (a pool's `source:` already has to name
    /// this entry, and this entry's `name` is what an operator points a
    /// `wayhouse-agent --name` at).
    origin: String,
    /// The `backend_sources[].pubkey` this config pins — every fetch
    /// verifies the registry's currently-registered pubkey still matches
    /// before trusting its backend list, so a name later re-registered
    /// under a different key is refused rather than silently trusted.
    pubkey: String,
    controller_url: String,
    token: Option<String>,
    interval: Duration,
    client: reqwest::Client,
    /// Whether the registry has served this origin since the last
    /// withdrawal. A `404` after that is a deletion (or lease expiry), not
    /// "not registered yet".
    seen: AtomicBool,
}

impl TunnelSource {
    pub fn new(
        pool: String,
        origin: String,
        pubkey: String,
        controller_url: String,
        token: Option<String>,
        interval: Duration,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            pool,
            origin,
            pubkey,
            controller_url,
            token,
            interval,
            client: wayhouse_http::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .context("build tunnel backend-peers HTTP client")?,
            seen: AtomicBool::new(false),
        })
    }
}

/// Only the fields this source needs from `wayhouse-controller`'s `GET
/// /peers/{name}` response — the full shape is `wayhouse_controller::peers::
/// PeerRegistration`, duplicated here the same way every other cross-process
/// wire shape in this codebase is (see `tunnel_client.rs`'s module doc).
#[derive(Deserialize)]
struct PeerRegistration {
    pubkey: String,
    #[serde(default)]
    backends: Vec<String>,
}

#[async_trait]
impl BackendSource for TunnelSource {
    fn pool(&self) -> &str {
        &self.pool
    }
    fn kind(&self) -> &'static str {
        "tunnel"
    }
    fn refresh_interval(&self) -> Duration {
        self.interval
    }

    async fn fetch(&self) -> Result<Vec<SocketAddr>, SourceError> {
        let url = format!(
            "{}/peers/{}",
            self.controller_url.trim_end_matches('/'),
            self.origin
        );
        let mut req = self.client.get(&url);
        if let Some(token) = &self.token {
            req = req.bearer_auth(token);
        }
        let resp = req.send().await.map_err(|e| SourceError::Unreachable {
            context: format!("fetching {url}"),
            cause: wayhouse_http::error_chain(&e),
        })?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            // An origin this source has seen is gone (deleted, or its lease
            // expired): say so once, so the pool is cleared instead of
            // frozen. Otherwise the origin hasn't registered yet (or ever) —
            // "no addresses known right now", and the level-triggered
            // contract keeps the last-known-good set for an empty `Ok`.
            if self.seen.swap(false, Ordering::Relaxed) {
                return Err(SourceError::Withdrawn {
                    origin: self.origin.clone(),
                });
            }
            return Ok(Vec::new());
        }
        if !resp.status().is_success() {
            return Err(SourceError::BadResponse {
                context: format!("controller {url}"),
                cause: format!("returned {}", resp.status()),
            });
        }

        let reg: PeerRegistration = resp.json().await.map_err(|e| SourceError::BadResponse {
            context: format!("parsing peer registration from {url}"),
            cause: wayhouse_http::error_chain(&e),
        })?;
        if reg.pubkey != self.pubkey {
            return Err(SourceError::PubkeyMismatch {
                origin: self.origin.clone(),
                expected: self.pubkey.clone(),
                got: reg.pubkey,
            });
        }

        self.seen.store(true, Ordering::Relaxed);
        let mut out = Vec::with_capacity(reg.backends.len());
        for b in &reg.backends {
            out.push(b.parse().map_err(|_| SourceError::BadResponse {
                context: format!("origin {:?}", self.origin),
                cause: format!("backend {b:?} is not a valid ip:port"),
            })?);
        }
        Ok(out)
    }
}
