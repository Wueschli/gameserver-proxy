//! Concrete backend-discovery adapters (phase 8).
//!
//! `gsp-core` owns the [`BackendSource`] seam and the level-triggered
//! reconcile; the network clients live here in the binary, the same split as
//! resolvers. Each adapter answers one question — "what is the current backend
//! address set for this pool?" — and `gsp-core` diffs it against the live set.
//!
//! - [`DnsSrvSource`] resolves an SRV record (port from the record).
//! - [`ConsulSource`] lists passing instances of a Consul service.
//! - [`KubernetesSource`] reads the Endpoints of a Kubernetes service and
//!   watches them, so a pod change lands in a fetch immediately; the polling
//!   interval remains as the resync safety net.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::{anyhow, Context};
use async_trait::async_trait;
use gsp_config::{Config, SourceConfig, SourceKind};
use gsp_core::{BackendSource, SourceError, SourceFactory};
use hickory_resolver::config::ResolverConfig;
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use hickory_resolver::TokioResolver;
use serde::Deserialize;
use tokio::sync::Notify;

/// In-pod service-account paths for the Kubernetes API.
const K8S_TOKEN_PATH: &str = "/var/run/secrets/kubernetes.io/serviceaccount/token";
const K8S_CA_PATH: &str = "/var/run/secrets/kubernetes.io/serviceaccount/ca.crt";

/// Which `gsp-controller` backend-peers registry a `tunnel` source resolves
/// against (phase 14 slice 5, `docs/11`) — the identical registry `gsp`'s
/// own `--tunnel-iface` reconcile task (`tunnel_client.rs`) subscribes to,
/// so this is built from the same `--tunnel-controller-url`/
/// `--tunnel-controller-token` flags rather than a second pair.
#[derive(Debug, Clone)]
pub struct TunnelRegistry {
    pub controller_url: String,
    pub token: Option<String>,
}

/// Build one [`BackendSource`] per pool that declares a dynamic `source`.
/// Used at startup for the best-effort initial fetch; the live [`SourceManager`]
/// rebuilds sources through [`DiscoveryFactory`] on reload.
pub fn build_sources(
    cfg: &Config,
    tunnel_registry: Option<&TunnelRegistry>,
) -> anyhow::Result<Vec<Arc<dyn BackendSource>>> {
    let kube_auth = KubeAuth::from_pod();
    let mut out: Vec<Arc<dyn BackendSource>> = Vec::new();
    for p in &cfg.pools {
        let Some(sc) = &p.source else { continue };
        out.push(build_one(&p.name, sc, &kube_auth, tunnel_registry)?);
    }
    Ok(out)
}

/// Build the concrete adapter for one pool's `source`.
pub fn build_one(
    pool: &str,
    sc: &SourceConfig,
    kube_auth: &KubeAuth,
    tunnel_registry: Option<&TunnelRegistry>,
) -> anyhow::Result<Arc<dyn BackendSource>> {
    let pool = pool.to_string();
    let src: Arc<dyn BackendSource> = match &sc.kind {
        SourceKind::DnsSrv { record } => Arc::new(DnsSrvSource::new(
            pool,
            record.clone(),
            sc.refresh_interval,
        )?),
        SourceKind::Consul { service, addr, tag } => Arc::new(ConsulSource::new(
            pool,
            service.clone(),
            addr.clone(),
            tag.clone(),
            sc.refresh_interval,
        )?),
        SourceKind::Kubernetes {
            namespace,
            service,
            port_name,
            api,
        } => Arc::new(KubernetesSource::new(
            pool,
            namespace.clone(),
            service.clone(),
            port_name.clone(),
            api.clone(),
            sc.refresh_interval,
            kube_auth.clone(),
        )?),
        SourceKind::Tunnel { pubkey } => {
            let registry = tunnel_registry.ok_or_else(|| {
                anyhow!(
                    "pool {pool}: backend_sources {} is type tunnel but \
                     --tunnel-controller-url is not set",
                    sc.name
                )
            })?;
            Arc::new(TunnelSource::new(
                pool,
                sc.name.clone(),
                pubkey.clone(),
                registry.controller_url.clone(),
                registry.token.clone(),
                sc.refresh_interval,
            )?)
        }
    };
    Ok(src)
}

/// [`SourceFactory`] for the live [`SourceManager`]: rebuilds a pool's adapter
/// from its (possibly changed) [`SourceConfig`] on reload. Holds the in-pod
/// Kubernetes credentials read once at startup.
pub struct DiscoveryFactory {
    kube_auth: KubeAuth,
    tunnel_registry: Option<TunnelRegistry>,
}

impl DiscoveryFactory {
    pub fn new(tunnel_registry: Option<TunnelRegistry>) -> Self {
        Self {
            kube_auth: KubeAuth::from_pod(),
            tunnel_registry,
        }
    }
}

impl SourceFactory for DiscoveryFactory {
    fn build(&self, pool: &str, cfg: &SourceConfig) -> Result<Arc<dyn BackendSource>, SourceError> {
        build_one(pool, cfg, &self.kube_auth, self.tunnel_registry.as_ref())
            .map_err(SourceError::build)
    }
}

// ---------------------------------------------------------------------------
// DNS SRV
// ---------------------------------------------------------------------------

pub struct DnsSrvSource {
    pool: String,
    record: String,
    interval: Duration,
    resolver: TokioResolver,
}

impl DnsSrvSource {
    pub fn new(pool: String, record: String, interval: Duration) -> anyhow::Result<Self> {
        // Prefer the host resolver config; fall back to a default (public) one
        // so a missing /etc/resolv.conf doesn't abort startup.
        let resolver = TokioResolver::builder_tokio()
            .and_then(hickory_resolver::ResolverBuilder::build)
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "dns_srv: system resolver config unavailable; using defaults");
                TokioResolver::builder_with_config(
                    ResolverConfig::default(),
                    TokioRuntimeProvider::default(),
                )
                .build()
                .expect("building a resolver from a default config never fails")
            });
        Ok(Self {
            pool,
            record,
            interval,
            resolver,
        })
    }

    /// Test constructor: resolve via a specific nameserver `ip:port` (UDP).
    #[cfg(test)]
    pub fn with_nameserver(
        pool: String,
        record: String,
        interval: Duration,
        nameserver: SocketAddr,
    ) -> Self {
        use hickory_resolver::config::{NameServerConfig, ResolverConfig};
        let mut ns = NameServerConfig::udp(nameserver.ip());
        ns.connections[0].port = nameserver.port();
        let cfg = ResolverConfig::from_parts(None, vec![], vec![ns]);
        Self {
            pool,
            record,
            interval,
            resolver: TokioResolver::builder_with_config(cfg, TokioRuntimeProvider::default())
                .build()
                .expect("building a resolver from a fixed nameserver never fails"),
        }
    }
}

#[async_trait]
impl BackendSource for DnsSrvSource {
    fn pool(&self) -> &str {
        &self.pool
    }
    fn kind(&self) -> &'static str {
        "dns_srv"
    }
    fn refresh_interval(&self) -> Duration {
        self.interval
    }

    async fn fetch(&self) -> Result<Vec<SocketAddr>, SourceError> {
        let lookup =
            self.resolver
                .srv_lookup(&self.record)
                .await
                .map_err(|e| SourceError::Unreachable {
                    context: format!("SRV lookup for {}", self.record),
                    cause: e.to_string(),
                })?;

        let mut out = Vec::new();
        for record in lookup.answers() {
            let hickory_resolver::proto::rr::RData::SRV(srv) = &record.data else {
                continue;
            };
            let port = srv.port;
            let target = srv.target.to_utf8();
            let target = target.trim_end_matches('.');
            match target.parse() {
                Ok(ip) => out.push(SocketAddr::new(ip, port)),
                Err(_) => {
                    let ips = self.resolver.lookup_ip(target).await.map_err(|e| {
                        SourceError::Unreachable {
                            context: format!("A/AAAA lookup for SRV target {target}"),
                            cause: e.to_string(),
                        }
                    })?;
                    for ip in ips.iter() {
                        out.push(SocketAddr::new(ip, port));
                    }
                }
            }
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Consul
// ---------------------------------------------------------------------------

pub struct ConsulSource {
    pool: String,
    service: String,
    base: String,
    tag: Option<String>,
    interval: Duration,
    client: reqwest::Client,
}

#[derive(Deserialize)]
struct ConsulEntry {
    #[serde(rename = "Node")]
    node: ConsulNode,
    #[serde(rename = "Service")]
    service: ConsulService,
}

#[derive(Deserialize)]
struct ConsulNode {
    #[serde(rename = "Address")]
    address: String,
}

#[derive(Deserialize)]
struct ConsulService {
    #[serde(rename = "Address")]
    address: String,
    #[serde(rename = "Port")]
    port: u16,
}

impl ConsulSource {
    #[allow(clippy::needless_pass_by_value)] // constructors take ownership of their settings
    pub fn new(
        pool: String,
        service: String,
        base: String,
        tag: Option<String>,
        interval: Duration,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            pool,
            service,
            base: base.trim_end_matches('/').to_string(),
            tag,
            interval,
            client: gsp_http::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .context("build Consul HTTP client")?,
        })
    }
}

#[async_trait]
impl BackendSource for ConsulSource {
    fn pool(&self) -> &str {
        &self.pool
    }
    fn kind(&self) -> &'static str {
        "consul"
    }
    fn refresh_interval(&self) -> Duration {
        self.interval
    }

    async fn fetch(&self) -> Result<Vec<SocketAddr>, SourceError> {
        let mut url = format!(
            "{}/v1/health/service/{}?passing=true",
            self.base, self.service
        );
        if let Some(tag) = &self.tag {
            url.push_str("&tag=");
            url.push_str(tag);
        }
        let entries: Vec<ConsulEntry> = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| SourceError::Unreachable {
                context: "Consul request".into(),
                cause: gsp_http::error_chain(&e),
            })?
            .error_for_status()
            .map_err(|e| SourceError::BadResponse {
                context: "Consul response status".into(),
                cause: gsp_http::error_chain(&e),
            })?
            .json()
            .await
            .map_err(|e| SourceError::BadResponse {
                context: "decode Consul response".into(),
                cause: gsp_http::error_chain(&e),
            })?;

        let mut out = Vec::new();
        for e in entries {
            let host = if e.service.address.is_empty() {
                e.node.address
            } else {
                e.service.address
            };
            out.extend(resolve_host_port(&host, e.service.port).await?);
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Kubernetes Endpoints (poll + watch)
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
pub struct KubeAuth {
    token: Option<String>,
    ca_pem: Option<Vec<u8>>,
}

impl KubeAuth {
    /// Load the service-account token and CA from the standard in-pod paths.
    /// Missing files are fine (e.g. an out-of-cluster test against a plain-HTTP
    /// mock) — the fields stay `None`.
    pub fn from_pod() -> Self {
        Self {
            token: std::fs::read_to_string(K8S_TOKEN_PATH)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            ca_pem: std::fs::read(K8S_CA_PATH).ok(),
        }
    }
}

pub struct KubernetesSource {
    pool: String,
    namespace: String,
    service: String,
    port_name: Option<String>,
    api: String,
    interval: Duration,
    token: Option<String>,
    client: reqwest::Client,
    /// Same credentials, but no total-request timeout: a watch is a long-lived
    /// response, bounded by [`WATCH_READ_TIMEOUT`] between chunks instead.
    watch_client: reqwest::Client,
    /// `metadata.resourceVersion` of the newest Endpoints state seen: set by
    /// every `fetch` and every watch event, cleared when the API server
    /// reports it expired. A watch resumes from it, so it only ever delivers
    /// changes made after the last fetch.
    resource_version: Mutex<Option<String>>,
    /// Permit stored by `fetch` when it sets `resource_version`; wakes a
    /// `changed()` that was waiting for one.
    version_ready: Notify,
}

/// Server-side lifetime of one watch request; it is reopened afterwards.
const WATCH_TIMEOUT_SECS: u64 = 300;
/// Client-side cap on silence from the API server (it sends bookmarks and
/// closes at [`WATCH_TIMEOUT_SECS`]), so a dead connection cannot hang the watch.
const WATCH_READ_TIMEOUT: Duration = Duration::from_secs(WATCH_TIMEOUT_SECS + 30);
/// Minimum time between two watch requests: backs off a failing or
/// immediately-closing watch while the poll tick keeps the set fresh. Failures
/// double it up to [`WATCH_RETRY_MAX`] (e.g. a missing `watch` grant).
const WATCH_RETRY: Duration = Duration::from_secs(5);
const WATCH_RETRY_MAX: Duration = Duration::from_secs(60);
/// Longest unterminated watch-event line we buffer.
const WATCH_LINE_MAX: usize = 16 * 1024 * 1024;

#[derive(Deserialize)]
struct Endpoints {
    #[serde(default)]
    metadata: ObjectMeta,
    #[serde(default)]
    subsets: Vec<Subset>,
}

#[derive(Deserialize, Default)]
struct ObjectMeta {
    #[serde(default, rename = "resourceVersion")]
    resource_version: Option<String>,
}

/// One line of a watch response (`{"type": ..., "object": ...}`). For an
/// `ERROR` event the object is a `Status`, whose `code` says why.
#[derive(Deserialize)]
struct WatchEvent {
    #[serde(rename = "type")]
    kind: String,
    object: WatchObject,
}

#[derive(Deserialize)]
struct WatchObject {
    #[serde(default)]
    metadata: ObjectMeta,
    #[serde(default)]
    code: Option<u16>,
    #[serde(default)]
    message: Option<String>,
}

/// How one watch request ended.
enum WatchEnd {
    /// The set may have changed: fetch it.
    Changed,
    /// The server closed the stream (its timeout); reopen from the last version.
    Closed,
    /// The resource version is too old (`410 Gone`); a fetch must renew it.
    Expired,
}

#[derive(Deserialize)]
struct Subset {
    #[serde(default)]
    addresses: Vec<EndpointAddress>,
    #[serde(default)]
    ports: Vec<EndpointPort>,
}

#[derive(Deserialize)]
struct EndpointAddress {
    ip: String,
}

#[derive(Deserialize)]
struct EndpointPort {
    #[serde(default)]
    name: Option<String>,
    port: u16,
}

impl KubernetesSource {
    #[allow(clippy::too_many_arguments)] // one call site (build_sources)
    #[allow(clippy::needless_pass_by_value)] // constructors take ownership of their settings
    pub fn new(
        pool: String,
        namespace: String,
        service: String,
        port_name: Option<String>,
        api: String,
        interval: Duration,
        auth: KubeAuth,
    ) -> anyhow::Result<Self> {
        let cert = auth
            .ca_pem
            .as_deref()
            .map(reqwest::Certificate::from_pem)
            .transpose()
            .context("parse Kubernetes CA cert")?;
        let builder = || {
            let b = gsp_http::builder();
            match &cert {
                Some(cert) => b.add_root_certificate(cert.clone()),
                None => b,
            }
        };
        Ok(Self {
            pool,
            namespace,
            service,
            port_name,
            api: api.trim_end_matches('/').to_string(),
            interval,
            token: auth.token,
            client: builder()
                .timeout(Duration::from_secs(5))
                .build()
                .context("build Kubernetes HTTP client")?,
            watch_client: builder()
                .connect_timeout(Duration::from_secs(5))
                .read_timeout(WATCH_READ_TIMEOUT)
                .build()
                .context("build Kubernetes watch client")?,
            resource_version: Mutex::new(None),
            version_ready: Notify::new(),
        })
    }

    fn endpoints_url(&self) -> String {
        format!(
            "{}/api/v1/namespaces/{}/endpoints",
            self.api, self.namespace
        )
    }

    fn version(&self) -> Option<String> {
        self.resource_version
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn set_version(&self, version: Option<String>) {
        let ready = version.is_some();
        *self
            .resource_version
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = version;
        if ready {
            self.version_ready.notify_one();
        }
    }

    /// One watch request from `version`: returns when an event reports a
    /// change, the server closes the stream, or the version has expired.
    async fn watch_once(&self, version: &str) -> anyhow::Result<WatchEnd> {
        let mut req = self.watch_client.get(self.endpoints_url()).query(&[
            ("watch", "true"),
            ("fieldSelector", &format!("metadata.name={}", self.service)),
            ("resourceVersion", version),
            ("allowWatchBookmarks", "true"),
            ("timeoutSeconds", &WATCH_TIMEOUT_SECS.to_string()),
        ]);
        if let Some(token) = &self.token {
            req = req.bearer_auth(token);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| anyhow!("Kubernetes watch request: {}", gsp_http::error_chain(&e)))?;
        if resp.status() == reqwest::StatusCode::GONE {
            return Ok(WatchEnd::Expired);
        }
        let mut resp = resp
            .error_for_status()
            .map_err(|e| anyhow!("Kubernetes watch status: {}", gsp_http::error_chain(&e)))?;

        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| anyhow!("Kubernetes watch stream: {}", gsp_http::error_chain(&e)))?
        {
            buf.extend_from_slice(&chunk);
            while let Some(end) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=end).collect();
                if line.trim_ascii().is_empty() {
                    continue;
                }
                let ev: WatchEvent = serde_json::from_slice(&line)
                    .map_err(|e| anyhow!("decode Kubernetes watch event: {e}"))?;
                match ev.kind.as_str() {
                    "BOOKMARK" => self.set_version(ev.object.metadata.resource_version),
                    "ERROR" if ev.object.code == Some(410) => return Ok(WatchEnd::Expired),
                    "ERROR" => {
                        return Err(anyhow!(
                            "Kubernetes watch error: {}",
                            ev.object.message.unwrap_or_default()
                        ))
                    }
                    _ => {
                        self.set_version(ev.object.metadata.resource_version);
                        return Ok(WatchEnd::Changed);
                    }
                }
            }
            if buf.len() > WATCH_LINE_MAX {
                return Err(anyhow!(
                    "Kubernetes watch event exceeds {WATCH_LINE_MAX} bytes"
                ));
            }
        }
        Ok(WatchEnd::Closed)
    }
}

#[async_trait]
impl BackendSource for KubernetesSource {
    fn pool(&self) -> &str {
        &self.pool
    }
    fn kind(&self) -> &'static str {
        "kubernetes"
    }
    fn refresh_interval(&self) -> Duration {
        self.interval
    }

    async fn changed(&self) {
        let mut warned = false;
        let mut retry = WATCH_RETRY;
        loop {
            // Nothing to resume from until a fetch has run (or after expiry).
            let Some(version) = self.version() else {
                self.version_ready.notified().await;
                continue;
            };
            let started = tokio::time::Instant::now();
            match self.watch_once(&version).await {
                Ok(WatchEnd::Changed) => return,
                Ok(WatchEnd::Closed) => {
                    warned = false;
                    retry = WATCH_RETRY;
                }
                Ok(WatchEnd::Expired) => {
                    // The version is too old to resume from: have `refresh_loop`
                    // fetch now, which also renews it. Cleared first, so a
                    // failing fetch makes the next call wait instead of spin.
                    tracing::debug!(pool = self.pool, "Kubernetes watch version expired");
                    self.set_version(None);
                    return;
                }
                Err(e) => {
                    // The poll tick still converges the set; say so once per outage.
                    if warned {
                        tracing::debug!(pool = self.pool, error = %e, "Kubernetes watch failed again");
                    } else {
                        tracing::warn!(
                            pool = self.pool, error = %e,
                            "Kubernetes watch failed; falling back to polling until it recovers"
                        );
                        warned = true;
                    }
                }
            }
            tokio::time::sleep_until(started + retry).await;
            if warned {
                retry = (retry * 2).min(WATCH_RETRY_MAX);
            }
        }
    }

    async fn fetch(&self) -> Result<Vec<SocketAddr>, SourceError> {
        let url = format!("{}/{}", self.endpoints_url(), self.service);
        let mut req = self.client.get(&url);
        if let Some(token) = &self.token {
            req = req.bearer_auth(token);
        }
        let ep: Endpoints = req
            .send()
            .await
            .map_err(|e| SourceError::Unreachable {
                context: "Kubernetes request".into(),
                cause: gsp_http::error_chain(&e),
            })?
            .error_for_status()
            .map_err(|e| SourceError::BadResponse {
                context: "Kubernetes response status".into(),
                cause: gsp_http::error_chain(&e),
            })?
            .json()
            .await
            .map_err(|e| SourceError::BadResponse {
                context: "decode Kubernetes Endpoints".into(),
                cause: gsp_http::error_chain(&e),
            })?;

        self.set_version(ep.metadata.resource_version.clone());

        let mut out = Vec::new();
        for subset in &ep.subsets {
            let port = match &self.port_name {
                Some(want) => subset
                    .ports
                    .iter()
                    .find(|p| p.name.as_deref() == Some(want)),
                None => subset.ports.first(),
            };
            let Some(port) = port else { continue };
            for addr in &subset.addresses {
                let ip = addr.ip.parse().map_err(|_| SourceError::BadResponse {
                    context: "Kubernetes Endpoints".into(),
                    cause: format!("endpoint address {:?} is not an IP", addr.ip),
                })?;
                out.push(SocketAddr::new(ip, port.port));
            }
        }
        Ok(out)
    }
}

/// Parse `host` as an IP, or resolve it (A/AAAA) and pair every result with
/// `port`.
async fn resolve_host_port(host: &str, port: u16) -> Result<Vec<SocketAddr>, SourceError> {
    if let Ok(ip) = host.parse() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| SourceError::Unreachable {
            context: format!("resolve {host}:{port}"),
            cause: e.to_string(),
        })?
        .collect();
    if addrs.is_empty() {
        return Err(SourceError::BadResponse {
            context: format!("resolve {host}:{port}"),
            cause: "no addresses".into(),
        });
    }
    Ok(addrs)
}

// ---------------------------------------------------------------------------
// Tunnel (phase 14 backend transport, docs/11) — resolves an origin's
// currently-registered backends from gsp-controller's backend-peers
// registry (phase 14 slice 2), the same registry gsp's own `--tunnel-iface`
// reconcile task subscribes to (`tunnel_client.rs`).
// ---------------------------------------------------------------------------

pub struct TunnelSource {
    pool: String,
    /// The `backend_sources[].name` — also this origin's registered name in
    /// the peers registry (`GET /peers/{origin}`); the two are the same
    /// identifier by construction (a pool's `source:` already has to name
    /// this entry, and this entry's `name` is what an operator points a
    /// `gsp-agent --name` at).
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
            client: gsp_http::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .context("build tunnel backend-peers HTTP client")?,
            seen: AtomicBool::new(false),
        })
    }
}

/// Only the fields this source needs from `gsp-controller`'s `GET
/// /peers/{name}` response — the full shape is `gsp_controller::peers::
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
            cause: gsp_http::error_chain(&e),
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
            cause: gsp_http::error_chain(&e),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A failed request's error text must carry its cause: `refresh_loop`
    /// logs it with `%e`, which for anyhow is only the outermost message.
    #[allow(clippy::needless_pass_by_value)] // constructors take ownership of their settings
    fn assert_names_the_cause(e: SourceError) {
        let text = e.to_string().to_lowercase();
        assert!(text.contains("connection refused"), "{text}");
    }

    const REFUSED: &str = "http://127.0.0.1:1";

    #[tokio::test]
    async fn consul_request_errors_name_the_cause() {
        let src = ConsulSource::new(
            "p".into(),
            "game".into(),
            REFUSED.into(),
            None,
            Duration::from_secs(10),
        )
        .unwrap();
        assert_names_the_cause(src.fetch().await.unwrap_err());
    }

    #[tokio::test]
    async fn kubernetes_request_errors_name_the_cause() {
        let src = KubernetesSource::new(
            "p".into(),
            "games".into(),
            "match".into(),
            None,
            REFUSED.into(),
            Duration::from_secs(10),
            KubeAuth::default(),
        )
        .unwrap();
        assert_names_the_cause(src.fetch().await.unwrap_err());
    }

    #[tokio::test]
    async fn consul_and_kubernetes_status_and_decode_errors_name_the_cause() {
        let bad_status = mock_http::serve_status("503 Service Unavailable", "").await;
        let not_json = mock_http::serve_json("not json").await;
        for (base, want) in [
            (format!("http://{}", bad_status.addr), "503"),
            (format!("http://{}", not_json.addr), "expected"),
        ] {
            let consul = ConsulSource::new(
                "p".into(),
                "game".into(),
                base.clone(),
                None,
                Duration::from_secs(10),
            )
            .unwrap();
            let e = consul.fetch().await.unwrap_err().to_string();
            assert!(e.contains(want), "consul: {e}");
            let kube = KubernetesSource::new(
                "p".into(),
                "games".into(),
                "match".into(),
                None,
                base,
                Duration::from_secs(10),
                KubeAuth::default(),
            )
            .unwrap();
            let e = kube.fetch().await.unwrap_err().to_string();
            assert!(e.contains(want), "kubernetes: {e}");
        }
    }

    #[tokio::test]
    async fn tunnel_decode_errors_name_the_cause() {
        let not_json = mock_http::serve_json("not json").await;
        let src = TunnelSource::new(
            "p".into(),
            "home".into(),
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
            format!("http://{}", not_json.addr),
            None,
            Duration::from_secs(10),
        )
        .unwrap();
        let e = src.fetch().await.unwrap_err().to_string();
        assert!(e.contains("expected"), "{e}");
    }

    #[tokio::test]
    async fn tunnel_request_errors_name_the_cause() {
        let src = TunnelSource::new(
            "p".into(),
            "home".into(),
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
            REFUSED.into(),
            None,
            Duration::from_secs(10),
        )
        .unwrap();
        assert_names_the_cause(src.fetch().await.unwrap_err());
    }

    #[tokio::test]
    async fn consul_source_lists_passing_instances() {
        let body = r#"[
          {"Node":{"Address":"10.0.0.9"},"Service":{"Address":"10.0.1.1","Port":7777}},
          {"Node":{"Address":"10.0.0.9"},"Service":{"Address":"","Port":7778}}
        ]"#;
        let server = mock_http::serve_json(body).await;
        let src = ConsulSource::new(
            "p".into(),
            "game".into(),
            format!("http://{}", server.addr),
            None,
            Duration::from_secs(10),
        )
        .unwrap();
        let mut got = src.fetch().await.unwrap();
        got.sort();
        assert_eq!(
            got,
            vec![
                "10.0.0.9:7778".parse().unwrap(),
                "10.0.1.1:7777".parse().unwrap(),
            ]
        );
    }

    #[tokio::test]
    async fn dns_srv_source_resolves_targets_to_addresses() {
        use hickory_server::proto::rr::rdata::{A, SRV};
        use hickory_server::proto::rr::{LowerName, Name, RData, Record};
        use hickory_server::server::Server;
        use hickory_server::store::in_memory::InMemoryZoneHandler;
        use hickory_server::zone_handler::{AxfrPolicy, Catalog, ZoneType};
        use std::sync::Arc;

        let origin = Name::from_ascii("example.com.").unwrap();
        let mut auth: InMemoryZoneHandler =
            InMemoryZoneHandler::empty(origin.clone(), ZoneType::Primary, AxfrPolicy::Deny);
        auth.upsert_mut(
            Record::from_rdata(
                Name::from_ascii("_game._udp.example.com.").unwrap(),
                60,
                RData::SRV(SRV::new(
                    0,
                    0,
                    7777,
                    Name::from_ascii("host1.example.com.").unwrap(),
                )),
            ),
            0,
        );
        auth.upsert_mut(
            Record::from_rdata(
                Name::from_ascii("host1.example.com.").unwrap(),
                60,
                RData::A(A::new(127, 0, 0, 1)),
            ),
            0,
        );

        let mut catalog = Catalog::new();
        catalog.upsert(LowerName::from(origin), vec![Arc::new(auth)]);

        let udp = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let ns = udp.local_addr().unwrap();
        let mut server = Server::new(catalog);
        server.register_socket(udp);
        tokio::spawn(async move {
            let _ = server.block_until_done().await;
        });

        let src = DnsSrvSource::with_nameserver(
            "p".into(),
            "_game._udp.example.com.".into(),
            Duration::from_secs(10),
            ns,
        );
        let got = src.fetch().await.unwrap();
        assert_eq!(got, vec!["127.0.0.1:7777".parse().unwrap()]);
    }

    #[tokio::test]
    async fn kubernetes_source_selects_named_port() {
        let body = r#"{"subsets":[
          {"addresses":[{"ip":"10.2.0.1"},{"ip":"10.2.0.2"}],
           "ports":[{"name":"metrics","port":9000},{"name":"game","port":7777}]}
        ]}"#;
        let server = mock_http::serve_json(body).await;
        let src = KubernetesSource::new(
            "p".into(),
            "games".into(),
            "match".into(),
            Some("game".into()),
            format!("http://{}", server.addr),
            Duration::from_secs(10),
            KubeAuth::default(),
        )
        .unwrap();
        let mut got = src.fetch().await.unwrap();
        got.sort();
        assert_eq!(
            got,
            vec![
                "10.2.0.1:7777".parse().unwrap(),
                "10.2.0.2:7777".parse().unwrap(),
            ]
        );
    }

    fn watching_source(addr: SocketAddr) -> KubernetesSource {
        KubernetesSource::new(
            "p".into(),
            "games".into(),
            "match".into(),
            None,
            format!("http://{addr}"),
            Duration::from_secs(3600),
            KubeAuth::default(),
        )
        .unwrap()
    }

    const LIST_V10: &str = r#"{"metadata":{"resourceVersion":"10"},"subsets":[
      {"addresses":[{"ip":"10.2.0.1"}],"ports":[{"port":7777}]}]}"#;

    fn event(kind: &str, version: &str) -> String {
        format!(r#"{{"type":"{kind}","object":{{"metadata":{{"resourceVersion":"{version}"}}}}}}"#)
    }

    async fn pending(src: &KubernetesSource) -> bool {
        tokio::time::timeout(Duration::from_millis(300), src.changed())
            .await
            .is_err()
    }

    #[tokio::test]
    async fn kubernetes_watch_resumes_from_the_fetched_version_and_signals_an_event() {
        let server = mock_http::serve_k8s(LIST_V10, vec![event("MODIFIED", "11")]).await;
        let src = watching_source(server.addr);

        // Nothing to resume from before the first fetch: no watch is opened.
        assert!(pending(&src).await);
        assert!(server.watch_requests().is_empty());

        src.fetch().await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), src.changed())
            .await
            .expect("an event ends the wait");

        let reqs = server.watch_requests();
        assert_eq!(reqs.len(), 1, "{reqs:?}");
        assert!(reqs[0].contains("watch=true"), "{}", reqs[0]);
        assert!(reqs[0].contains("resourceVersion=10"), "{}", reqs[0]);
        assert!(
            reqs[0].contains("fieldSelector=metadata.name%3Dmatch"),
            "{}",
            reqs[0]
        );
        assert_eq!(
            src.version().as_deref(),
            Some("11"),
            "events advance the version"
        );
    }

    #[tokio::test]
    async fn kubernetes_watch_bookmarks_advance_the_version_without_signalling() {
        let server = mock_http::serve_k8s(LIST_V10, vec![event("BOOKMARK", "12")]).await;
        let src = watching_source(server.addr);
        src.fetch().await.unwrap();

        assert!(pending(&src).await, "a bookmark is not a change");
        assert_eq!(src.version().as_deref(), Some("12"));
    }

    #[tokio::test]
    async fn kubernetes_watch_asks_for_a_fetch_when_the_version_expires() {
        let gone = r#"{"type":"ERROR","object":{"kind":"Status","code":410,"message":"too old"}}"#;
        let server = mock_http::serve_k8s(LIST_V10, vec![gone.to_string()]).await;
        let src = watching_source(server.addr);
        src.fetch().await.unwrap();

        // Expiry resolves at once, so the loop refetches without the tick.
        tokio::time::timeout(Duration::from_secs(2), src.changed())
            .await
            .expect("expiry asks for a fetch");
        assert_eq!(src.version(), None);

        // Until that fetch renews the version, no watch is opened.
        assert!(pending(&src).await);
        assert_eq!(
            server.watch_requests().len(),
            1,
            "no retry on a stale version"
        );

        // The fetch renews the version and the watch resumes (the mock expires
        // it again, which asks for another fetch).
        src.fetch().await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), src.changed())
            .await
            .expect("a renewed version is watched again");
        assert_eq!(server.watch_requests().len(), 2);
    }

    #[tokio::test]
    async fn tunnel_source_returns_the_registered_backends_when_the_pubkey_matches() {
        let body = r#"{"name":"home","pubkey":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=","backends":["10.60.0.2:1","10.60.0.2:2"]}"#;
        let server = mock_http::serve_json(body).await;
        let src = TunnelSource::new(
            "p".into(),
            "home".into(),
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
            format!("http://{}", server.addr),
            None,
            Duration::from_secs(10),
        )
        .unwrap();
        let mut got = src.fetch().await.unwrap();
        got.sort();
        assert_eq!(
            got,
            vec![
                "10.60.0.2:1".parse().unwrap(),
                "10.60.0.2:2".parse().unwrap()
            ]
        );
    }

    #[tokio::test]
    async fn tunnel_source_refuses_a_registration_under_a_different_pubkey() {
        let body = r#"{"name":"home","pubkey":"BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB=","backends":["10.60.0.2:1"]}"#;
        let server = mock_http::serve_json(body).await;
        let src = TunnelSource::new(
            "p".into(),
            "home".into(),
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
            format!("http://{}", server.addr),
            None,
            Duration::from_secs(10),
        )
        .unwrap();
        assert!(src.fetch().await.is_err());
    }

    #[tokio::test]
    async fn tunnel_source_treats_a_never_registered_origin_as_no_addresses_not_an_error() {
        let server = mock_http::serve_status("404 Not Found", r#"{"error":"no peer"}"#).await;
        let src = TunnelSource::new(
            "p".into(),
            "home".into(),
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
            format!("http://{}", server.addr),
            None,
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(src.fetch().await.unwrap(), Vec::new());
    }

    #[tokio::test]
    async fn tunnel_source_withdraws_an_origin_that_vanishes_after_being_seen() {
        let seen = r#"{"name":"home","pubkey":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=","backends":["10.60.0.2:1"]}"#;
        let server = mock_http::serve_script(&[
            ("200 OK", seen),
            ("404 Not Found", r#"{"error":"no peer"}"#),
        ])
        .await;
        let src = TunnelSource::new(
            "p".into(),
            "home".into(),
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
            format!("http://{}", server.addr),
            None,
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(src.fetch().await.unwrap().len(), 1);
        assert!(matches!(
            src.fetch().await,
            Err(SourceError::Withdrawn { .. })
        ));
        // The withdrawal is reported once; a later 404 is "not registered".
        assert_eq!(src.fetch().await.unwrap(), Vec::new());
    }

    #[tokio::test]
    async fn tunnel_source_rejects_a_malformed_backend_address() {
        let body = r#"{"name":"home","pubkey":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=","backends":["not-an-addr"]}"#;
        let server = mock_http::serve_json(body).await;
        let src = TunnelSource::new(
            "p".into(),
            "home".into(),
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
            format!("http://{}", server.addr),
            None,
            Duration::from_secs(10),
        )
        .unwrap();
        assert!(src.fetch().await.is_err());
    }

    /// Minimal one-shot HTTP/1.1 server that replies to any request with a
    /// fixed JSON body. Enough for the Consul / Kubernetes adapters.
    pub(super) mod mock_http {
        use std::net::SocketAddr;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        pub struct Server {
            pub addr: SocketAddr,
        }

        pub async fn serve_json(body: &str) -> Server {
            serve_status("200 OK", body).await
        }

        /// As [`serve_json`], with a caller-chosen status line — for testing
        /// a non-200 response (e.g. `TunnelSource`'s `404` "not registered
        /// yet" case).
        pub async fn serve_status(status_line: &str, body: &str) -> Server {
            serve_script(&[(status_line, body)]).await
        }

        /// Answers the nth request with the nth `(status line, body)`, and
        /// every request past the end with the last one.
        pub async fn serve_script(script: &[(&str, &str)]) -> Server {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let script: Vec<(String, String)> = script
                .iter()
                .map(|(s, b)| ((*s).to_string(), (*b).to_string()))
                .collect();
            let served = Arc::new(AtomicUsize::new(0));
            tokio::spawn(async move {
                loop {
                    let Ok((mut sock, _)) = listener.accept().await else {
                        return;
                    };
                    let n = served.fetch_add(1, Ordering::SeqCst).min(script.len() - 1);
                    let (status_line, body) = script[n].clone();
                    tokio::spawn(async move {
                        let mut buf = [0u8; 2048];
                        let _ = sock.read(&mut buf).await;
                        let resp = format!(
                            "HTTP/1.1 {status_line}\r\ncontent-type: application/json\r\n\
                             content-length: {}\r\nconnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        let _ = sock.write_all(resp.as_bytes()).await;
                        let _ = sock.shutdown().await;
                    });
                }
            });
            Server { addr }
        }

        pub struct K8sServer {
            pub addr: SocketAddr,
            requests: Arc<Mutex<Vec<String>>>,
        }

        impl K8sServer {
            /// Request lines of every watch request so far.
            pub fn watch_requests(&self) -> Vec<String> {
                let log = self.requests.lock().unwrap();
                log.iter()
                    .filter(|r| r.contains("watch=true"))
                    .cloned()
                    .collect()
            }
        }

        /// A Kubernetes API stand-in: a plain `GET` answers with `list`; a
        /// `watch=true` request streams `events` (one JSON line each) and then
        /// holds the connection open.
        pub async fn serve_k8s(list: &str, events: Vec<String>) -> K8sServer {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let log = requests.clone();
            let list = list.to_string();
            tokio::spawn(async move {
                loop {
                    let Ok((mut sock, _)) = listener.accept().await else {
                        return;
                    };
                    let (log, list, events) = (log.clone(), list.clone(), events.clone());
                    tokio::spawn(async move {
                        let mut buf = [0u8; 4096];
                        let n = sock.read(&mut buf).await.unwrap_or(0);
                        let head = String::from_utf8_lossy(&buf[..n]).to_string();
                        let line = head.lines().next().unwrap_or_default().to_string();
                        log.lock().unwrap().push(line.clone());
                        if line.contains("watch=true") {
                            let _ = sock
                                .write_all(b"HTTP/1.1 200 OK\r\nconnection: close\r\n\r\n")
                                .await;
                            for ev in &events {
                                let _ = sock.write_all(format!("{ev}\n").as_bytes()).await;
                            }
                            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        } else {
                            let resp = format!(
                                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                                 content-length: {}\r\nconnection: close\r\n\r\n{}",
                                list.len(),
                                list
                            );
                            let _ = sock.write_all(resp.as_bytes()).await;
                        }
                        let _ = sock.shutdown().await;
                    });
                }
            });
            K8sServer { addr, requests }
        }
    }
}
