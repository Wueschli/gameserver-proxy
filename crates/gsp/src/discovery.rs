//! Concrete backend-discovery adapters (phase 8).
//!
//! `gsp-core` owns the [`BackendSource`] seam and the level-triggered
//! reconcile; the network clients live here in the binary, the same split as
//! resolvers. Each adapter answers one question — "what is the current backend
//! address set for this pool?" — and `gsp-core` diffs it against the live set.
//!
//! - [`DnsSrvSource`] (`dns_srv.rs`, the `dns-srv` cargo feature) resolves an
//!   SRV record (port from the record).
//! - [`ConsulSource`] lists passing instances of a Consul service, and blocks
//!   on its index so a health change lands in a fetch immediately.
//! - [`KubernetesSource`] reads the EndpointSlices of a Kubernetes service and
//!   watches them, so a pod change lands in a fetch immediately; the polling
//!   interval remains as the resync safety net.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use crate::dns_srv::DnsSrvSource;
use crate::tunnel_source::TunnelRegistry;
use anyhow::{anyhow, Context};
use async_trait::async_trait;
use gsp_config::{Config, SourceConfig, SourceKind};
use gsp_core::{BackendSource, SourceError, SourceFactory};
use serde::Deserialize;
use tokio::sync::Notify;

/// In-pod service-account paths for the Kubernetes API.
const K8S_TOKEN_PATH: &str = "/var/run/secrets/kubernetes.io/serviceaccount/token";
const K8S_CA_PATH: &str = "/var/run/secrets/kubernetes.io/serviceaccount/ca.crt";

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
        SourceKind::Consul {
            service,
            addr,
            tag,
            token_file,
        } => {
            if let Some(path) = token_file
                .as_ref()
                .filter(|p| !std::path::Path::new(p).exists())
            {
                tracing::warn!(
                    pool,
                    path,
                    "consul_token_file does not exist; Consul requests go out without a token"
                );
            }
            Arc::new(ConsulSource::new(
                pool,
                service.clone(),
                addr,
                tag.clone(),
                token_file.as_ref().map(TokenFile::new),
                sc.refresh_interval,
            )?)
        }
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
        SourceKind::Tunnel { pubkey } => crate::tunnel_source::build(
            pool,
            &sc.name,
            pubkey,
            tunnel_registry,
            sc.refresh_interval,
        )?,
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
// Credentials re-read from a file
// ---------------------------------------------------------------------------

/// How long a token read from a file is reused before the file is read again.
const TOKEN_REREAD: Duration = Duration::from_secs(60);

/// A bearer/ACL token kept as a path, not as contents: projected
/// service-account tokens and secret-manager files are rotated under a running
/// process, so the file is read again once [`TOKEN_REREAD`] has passed. A read
/// that fails keeps the last token (the file is briefly absent mid-rotation).
#[derive(Clone)]
pub struct TokenFile(Arc<TokenFileInner>);

struct TokenFileInner {
    path: std::path::PathBuf,
    reread: Duration,
    cached: Mutex<Option<(std::time::Instant, Option<String>)>>,
}

impl TokenFile {
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self::with_reread(path, TOKEN_REREAD)
    }

    fn with_reread(path: impl Into<std::path::PathBuf>, reread: Duration) -> Self {
        Self(Arc::new(TokenFileInner {
            path: path.into(),
            reread,
            cached: Mutex::new(None),
        }))
    }

    /// The current token; `None` if the file is empty or was never readable.
    pub fn get(&self) -> Option<String> {
        let inner = &*self.0;
        let mut cached = inner.cached.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((at, token)) = &*cached {
            if at.elapsed() < inner.reread {
                return token.clone();
            }
        }
        let previous = cached.take().and_then(|(_, token)| token);
        let token = match std::fs::read_to_string(&inner.path) {
            Ok(s) => Some(s.trim().to_string()).filter(|s| !s.is_empty()),
            Err(e) => {
                tracing::warn!(
                    path = %inner.path.display(), error = %e,
                    "cannot read token file; keeping the previous token"
                );
                previous
            }
        };
        *cached = Some((std::time::Instant::now(), token.clone()));
        token
    }
}

// ---------------------------------------------------------------------------
// Push signals shared by the watching sources
// ---------------------------------------------------------------------------

/// How one watch request (a Kubernetes watch, a Consul blocking query) ended.
enum WatchEnd {
    /// The set may have changed: fetch it.
    Changed,
    /// The server ended the request at its own timeout; ask again from the
    /// same position.
    Closed,
    /// The position (resource version, index) is no longer usable; a fetch
    /// must renew it.
    Expired,
}

/// Minimum time between two watch requests: backs off a failing or
/// immediately-closing watch while the poll tick keeps the set fresh. Failures
/// double it up to [`WATCH_RETRY_MAX`] (e.g. a missing `watch` grant).
const WATCH_RETRY: Duration = Duration::from_secs(5);
const WATCH_RETRY_MAX: Duration = Duration::from_secs(60);
/// A watch that "times out" faster than this was not really waiting (a server
/// that ignores `wait`): it is paced like a failing one.
const WATCH_FAST_CLOSE: Duration = Duration::from_secs(1);

/// Retry pacing and log noise for one source's watch: warns once per outage,
/// and doubles the delay between attempts while it keeps failing.
struct WatchBackoff<'a> {
    what: &'static str,
    pool: &'a str,
    warned: bool,
    retry: Duration,
}

impl<'a> WatchBackoff<'a> {
    fn new(what: &'static str, pool: &'a str) -> Self {
        Self {
            what,
            pool,
            warned: false,
            retry: WATCH_RETRY,
        }
    }

    /// A request ended cleanly: the outage, if any, is over.
    fn recovered(&mut self) {
        self.warned = false;
        self.retry = WATCH_RETRY;
    }

    /// A request ended without an event before it should have, so a retry loop
    /// would hammer the server: back off like a failure.
    fn closed_early(&mut self) {
        if !self.warned {
            tracing::warn!(
                pool = self.pool,
                "{} watch ends before its wait runs out; backing off (polling still converges)",
                self.what
            );
            self.warned = true;
        }
    }

    /// A request failed. The poll tick still converges the set; say so once
    /// per outage.
    fn failed(&mut self, e: &anyhow::Error) {
        if self.warned {
            tracing::debug!(pool = self.pool, error = %e, "{} watch failed again", self.what);
        } else {
            tracing::warn!(
                pool = self.pool, error = %e,
                "{} watch failed; falling back to polling until it recovers", self.what
            );
            self.warned = true;
        }
    }

    /// Minimum time from the start of the failed (or closed) request to the
    /// next one; doubles for the next call while an outage lasts.
    fn delay(&mut self) -> Duration {
        let now = self.retry;
        if self.warned {
            self.retry = (self.retry * 2).min(WATCH_RETRY_MAX);
        }
        now
    }
}

// ---------------------------------------------------------------------------
// Consul
// ---------------------------------------------------------------------------

pub struct ConsulSource {
    pool: String,
    service: String,
    base: reqwest::Url,
    tag: Option<String>,
    /// ACL token file, sent as `X-Consul-Token`.
    token: Option<TokenFile>,
    interval: Duration,
    client: reqwest::Client,
    /// Same as `client`, but the request timeout covers a whole blocking query
    /// ([`CONSUL_WAIT_SECS`] plus Consul's jitter) instead of 5 s.
    watch_client: reqwest::Client,
    /// `X-Consul-Index` of the newest answer seen: set by every `fetch`, and
    /// by a blocking query that returned a new one. The next blocking query
    /// waits on it. `None` until a fetch has run, or when Consul's index
    /// misbehaved (see `watch_once`).
    index: Mutex<Option<u64>>,
    /// Permit stored by `fetch` when it sets `index`; wakes a `changed()` that
    /// was waiting for one.
    index_ready: Notify,
}

/// How long Consul may hold one blocking query open (its default, and ten
/// minutes is the cap); the query is reissued afterwards.
const CONSUL_WAIT_SECS: u64 = 300;
/// Consul adds up to `wait / 16` of jitter to a blocking query; the HTTP
/// timeout leaves room for that and a slow answer.
const CONSUL_WATCH_TIMEOUT: Duration = Duration::from_secs(CONSUL_WAIT_SECS + 60);

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
        base: &str,
        tag: Option<String>,
        token: Option<TokenFile>,
        interval: Duration,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            pool,
            service,
            base: reqwest::Url::parse(base)
                .with_context(|| format!("Consul address {base:?} is not a URL"))?,
            tag,
            token,
            interval,
            client: gsp_http::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .context("build Consul HTTP client")?,
            watch_client: gsp_http::builder()
                .timeout(CONSUL_WATCH_TIMEOUT)
                .build()
                .context("build Consul watch HTTP client")?,
            index: Mutex::new(None),
            index_ready: Notify::new(),
        })
    }

    /// The health query; with `index`, a blocking one that Consul holds open
    /// until the result changes past it or [`CONSUL_WAIT_SECS`] runs out. The
    /// service and tag are escaped, whatever characters they hold.
    fn health_request(
        &self,
        client: &reqwest::Client,
        index: Option<u64>,
    ) -> anyhow::Result<reqwest::RequestBuilder> {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .map_err(|()| anyhow!("Consul address {} cannot carry a path", self.base))?
            .pop_if_empty()
            .extend(["v1", "health", "service", &self.service]);
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("passing", "true");
            if let Some(tag) = &self.tag {
                q.append_pair("tag", tag);
            }
            if let Some(index) = index {
                q.append_pair("index", &index.to_string());
                q.append_pair("wait", &format!("{CONSUL_WAIT_SECS}s"));
            }
        }
        let mut req = client.get(url);
        if let Some(token) = self.token.as_ref().and_then(TokenFile::get) {
            let mut value = reqwest::header::HeaderValue::from_str(&token)
                .map_err(|_| anyhow!("Consul token is not a valid header value"))?;
            value.set_sensitive(true); // kept out of debug output
            req = req.header("X-Consul-Token", value);
        }
        Ok(req)
    }

    fn index(&self) -> Option<u64> {
        *self.index.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn set_index(&self, index: Option<u64>) {
        let ready = index.is_some();
        *self.index.lock().unwrap_or_else(PoisonError::into_inner) = index;
        if ready {
            self.index_ready.notify_one();
        }
    }

    /// One blocking query from `index`: returns once Consul reports a new
    /// index (the passing set may have changed), or what it answered at the
    /// end of its wait.
    async fn watch_once(&self, index: u64) -> anyhow::Result<WatchEnd> {
        let resp = self
            .health_request(&self.watch_client, Some(index))?
            .send()
            .await
            .map_err(|e| anyhow!("Consul watch request: {}", gsp_http::error_chain(&e)))?
            .error_for_status()
            .map_err(|e| anyhow!("Consul watch status: {}", gsp_http::error_chain(&e)))?;
        // The body is not needed: a change is answered by a `fetch`, so the
        // blocking query only has to carry the index.
        let new = consul_index(&resp);
        Ok(match new {
            Some(new) if new == index => WatchEnd::Closed, // wait ran out
            Some(new) if new > index => {
                self.set_index(Some(new));
                WatchEnd::Changed
            }
            // Consul's contract: an index that goes backwards (or is missing
            // or zero) means the old one is meaningless; start over from a
            // fresh fetch.
            _ => {
                self.set_index(None);
                WatchEnd::Expired
            }
        })
    }
}

/// `X-Consul-Index` of a response: `None` if absent, unparsable or zero.
fn consul_index(resp: &reqwest::Response) -> Option<u64> {
    resp.headers()
        .get("x-consul-index")?
        .to_str()
        .ok()?
        .parse()
        .ok()
        .filter(|&i| i > 0)
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

    async fn changed(&self) {
        let mut backoff = WatchBackoff::new("Consul", &self.pool);
        loop {
            // Nothing to block on until a fetch has run (or after a reset).
            let Some(index) = self.index() else {
                self.index_ready.notified().await;
                continue;
            };
            let started = tokio::time::Instant::now();
            match self.watch_once(index).await {
                Ok(WatchEnd::Changed) => return,
                // Consul answered at the end of its wait: ask again at once, unless
                // it answered long before that.
                Ok(WatchEnd::Closed) if started.elapsed() >= WATCH_FAST_CLOSE => {
                    backoff.recovered();
                    continue;
                }
                Ok(WatchEnd::Closed) => backoff.closed_early(),
                Ok(WatchEnd::Expired) => {
                    tracing::debug!(pool = self.pool, "Consul index reset");
                    return;
                }
                Err(e) => backoff.failed(&e),
            }
            tokio::time::sleep_until(started + backoff.delay()).await;
        }
    }

    async fn fetch(&self) -> Result<Vec<SocketAddr>, SourceError> {
        let req =
            self.health_request(&self.client, None)
                .map_err(|e| SourceError::BadResponse {
                    context: "Consul request".into(),
                    cause: e.to_string(),
                })?;
        let resp = req
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
            })?;
        // The index this answer is valid at: a blocking query resumes from it.
        let index = consul_index(&resp);
        let entries: Vec<ConsulEntry> =
            resp.json().await.map_err(|e| SourceError::BadResponse {
                context: "decode Consul response".into(),
                cause: gsp_http::error_chain(&e),
            })?;
        self.set_index(index);

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
// Kubernetes EndpointSlices (poll + watch)
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
pub struct KubeAuth {
    token: Option<TokenFile>,
    ca_pem: Option<Vec<u8>>,
}

impl KubeAuth {
    /// The service-account token and CA at the standard in-pod paths. The
    /// token is read again as the kubelet rotates it (bound tokens last about
    /// an hour); the CA is read once. Missing files are fine (e.g. an
    /// out-of-cluster test against a plain-HTTP mock) — the fields stay `None`.
    pub fn from_pod() -> Self {
        Self {
            token: std::path::Path::new(K8S_TOKEN_PATH)
                .exists()
                .then(|| TokenFile::new(K8S_TOKEN_PATH)),
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
    token: Option<TokenFile>,
    client: reqwest::Client,
    /// Same credentials, but no total-request timeout: a watch is a long-lived
    /// response, bounded by [`WATCH_READ_TIMEOUT`] between chunks instead.
    watch_client: reqwest::Client,
    /// `metadata.resourceVersion` of the newest EndpointSlice state seen: set by
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
/// Longest unterminated watch-event line we buffer.
const WATCH_LINE_MAX: usize = 16 * 1024 * 1024;

/// `discovery.k8s.io/v1` `EndpointSliceList`: every slice of the service.
#[derive(Deserialize)]
struct EndpointSliceList {
    #[serde(default)]
    metadata: ObjectMeta,
    #[serde(default)]
    items: Vec<EndpointSlice>,
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

#[derive(Deserialize)]
struct EndpointSlice {
    /// `IPv4`, `IPv6` or `FQDN`.
    #[serde(default, rename = "addressType")]
    address_type: String,
    #[serde(default)]
    endpoints: Vec<SliceEndpoint>,
    #[serde(default)]
    ports: Vec<SlicePort>,
}

#[derive(Deserialize)]
struct SliceEndpoint {
    #[serde(default)]
    addresses: Vec<String>,
    #[serde(default)]
    conditions: SliceConditions,
}

#[derive(Deserialize, Default)]
struct SliceConditions {
    /// Absent means "unknown", which consumers must treat as ready.
    #[serde(default)]
    ready: Option<bool>,
}

#[derive(Deserialize)]
struct SlicePort {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    port: Option<u16>,
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

    fn slices_url(&self) -> String {
        format!(
            "{}/apis/discovery.k8s.io/v1/namespaces/{}/endpointslices",
            self.api, self.namespace
        )
    }

    /// Selects the slices the EndpointSlice controller made for the service.
    fn slice_selector(&self) -> String {
        format!("kubernetes.io/service-name={}", self.service)
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
        let mut req = self.watch_client.get(self.slices_url()).query(&[
            ("watch", "true"),
            ("labelSelector", &self.slice_selector()),
            ("resourceVersion", version),
            ("allowWatchBookmarks", "true"),
            ("timeoutSeconds", &WATCH_TIMEOUT_SECS.to_string()),
        ]);
        if let Some(token) = self.token.as_ref().and_then(TokenFile::get) {
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
        let mut backoff = WatchBackoff::new("Kubernetes", &self.pool);
        loop {
            // Nothing to resume from until a fetch has run (or after expiry).
            let Some(version) = self.version() else {
                self.version_ready.notified().await;
                continue;
            };
            let started = tokio::time::Instant::now();
            match self.watch_once(&version).await {
                Ok(WatchEnd::Changed) => return,
                Ok(WatchEnd::Closed) => backoff.recovered(),
                Ok(WatchEnd::Expired) => {
                    // The version is too old to resume from: have `refresh_loop`
                    // fetch now, which also renews it. Cleared first, so a
                    // failing fetch makes the next call wait instead of spin.
                    tracing::debug!(pool = self.pool, "Kubernetes watch version expired");
                    self.set_version(None);
                    return;
                }
                Err(e) => backoff.failed(&e),
            }
            tokio::time::sleep_until(started + backoff.delay()).await;
        }
    }

    async fn fetch(&self) -> Result<Vec<SocketAddr>, SourceError> {
        let mut req = self
            .client
            .get(self.slices_url())
            .query(&[("labelSelector", self.slice_selector())]);
        if let Some(token) = self.token.as_ref().and_then(TokenFile::get) {
            req = req.bearer_auth(token);
        }
        let list: EndpointSliceList = req
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
                context: "decode Kubernetes EndpointSlices".into(),
                cause: gsp_http::error_chain(&e),
            })?;

        // The list's own version is the point a watch resumes from.
        self.set_version(list.metadata.resource_version.clone());

        let mut out = Vec::new();
        for slice in &list.items {
            if slice.address_type == "FQDN" {
                continue; // not an address: the proxy dials IPs
            }
            let port = match &self.port_name {
                Some(want) => slice.ports.iter().find(|p| p.name.as_deref() == Some(want)),
                None => slice.ports.first(),
            };
            let Some(port) = port.and_then(|p| p.port) else {
                continue;
            };
            for ep in slice
                .endpoints
                .iter()
                .filter(|e| e.conditions.ready != Some(false))
            {
                for addr in &ep.addresses {
                    let ip = addr.parse().map_err(|_| SourceError::BadResponse {
                        context: "Kubernetes EndpointSlices".into(),
                        cause: format!("endpoint address {addr:?} is not an IP"),
                    })?;
                    let sa = SocketAddr::new(ip, port);
                    // A service can list one address in several slices.
                    if !out.contains(&sa) {
                        out.push(sa);
                    }
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "tunnel")]
    use crate::tunnel_source::TunnelSource;

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
            REFUSED,
            None,
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
                &base,
                None,
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

    #[cfg(feature = "tunnel")]
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

    #[cfg(feature = "tunnel")]
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
            &format!("http://{}", server.addr),
            None,
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
    async fn kubernetes_source_selects_named_port() {
        let body = r#"{"items":[
          {"addressType":"IPv4",
           "endpoints":[{"addresses":["10.2.0.1"]},{"addresses":["10.2.0.2"]}],
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

    #[tokio::test]
    async fn kubernetes_source_merges_slices_and_skips_unready_endpoints() {
        // Two slices (one per address family, as for a dual-stack service),
        // a duplicate across slices, an unready and a terminating endpoint,
        // an FQDN slice, and a slice without the wanted port.
        let body = r#"{"items":[
          {"addressType":"IPv4",
           "endpoints":[
             {"addresses":["10.2.0.1"]},
             {"addresses":["10.2.0.2"],"conditions":{"ready":true}},
             {"addresses":["10.2.0.3"],"conditions":{"ready":false}},
             {"addresses":["10.2.0.4"],"conditions":{"ready":false,"terminating":true}}],
           "ports":[{"name":"game","port":7777}]},
          {"addressType":"IPv4",
           "endpoints":[{"addresses":["10.2.0.1"]},{"addresses":["10.2.0.5"]}],
           "ports":[{"name":"game","port":7777}]},
          {"addressType":"IPv6",
           "endpoints":[{"addresses":["fd00::1"]}],
           "ports":[{"name":"game","port":7777}]},
          {"addressType":"FQDN",
           "endpoints":[{"addresses":["db.example.com"]}],
           "ports":[{"name":"game","port":7777}]},
          {"addressType":"IPv4",
           "endpoints":[{"addresses":["10.2.0.9"]}],
           "ports":[{"name":"other","port":1}]}
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
        let mut want: Vec<SocketAddr> = [
            "10.2.0.1:7777",
            "10.2.0.2:7777",
            "10.2.0.5:7777",
            "[fd00::1]:7777",
        ]
        .iter()
        .map(|a| a.parse().unwrap())
        .collect();
        want.sort();
        assert_eq!(got, want);
    }

    #[tokio::test]
    async fn kubernetes_source_lists_the_services_slices_by_label() {
        let server = mock_http::serve_k8s(LIST_V10, vec![]).await;
        let src = watching_source(server.addr);
        src.fetch().await.unwrap();
        let reqs = server.requests();
        assert_eq!(reqs.len(), 1, "{reqs:?}");
        assert!(
            reqs[0].starts_with("GET /apis/discovery.k8s.io/v1/namespaces/games/endpointslices?"),
            "{}",
            reqs[0]
        );
        assert!(
            reqs[0].contains("labelSelector=kubernetes.io%2Fservice-name%3Dmatch"),
            "{}",
            reqs[0]
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

    const LIST_V10: &str = r#"{"metadata":{"resourceVersion":"10"},"items":[
      {"addressType":"IPv4","endpoints":[{"addresses":["10.2.0.1"]}],
       "ports":[{"port":7777}]}]}"#;

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
            reqs[0].contains("labelSelector=kubernetes.io%2Fservice-name%3Dmatch"),
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

    const CONSUL_BODY: &str =
        r#"[{"Node":{"Address":"10.0.0.9"},"Service":{"Address":"10.0.1.1","Port":7777}}]"#;

    fn blocking_consul(addr: SocketAddr) -> ConsulSource {
        ConsulSource::new(
            "p".into(),
            "game".into(),
            &format!("http://{addr}"),
            Some("eu".into()),
            None,
            Duration::from_secs(3600),
        )
        .unwrap()
    }

    async fn consul_pending(src: &ConsulSource) -> bool {
        tokio::time::timeout(Duration::from_millis(300), src.changed())
            .await
            .is_err()
    }

    #[tokio::test]
    async fn consul_blocks_on_the_fetched_index_and_signals_a_new_one() {
        let server = mock_http::serve_consul(Some(5), CONSUL_BODY, vec![(0, Some(6))]).await;
        let src = blocking_consul(server.addr);

        // Nothing to block on before the first fetch: no request is made.
        assert!(consul_pending(&src).await);
        assert!(server.blocking_requests().is_empty());

        src.fetch().await.unwrap();
        assert_eq!(src.index(), Some(5));
        tokio::time::timeout(Duration::from_secs(2), src.changed())
            .await
            .expect("a new index ends the wait");

        let reqs = server.blocking_requests();
        assert_eq!(reqs.len(), 1, "{reqs:?}");
        assert!(reqs[0].contains("index=5"), "{}", reqs[0]);
        assert!(reqs[0].contains("wait=300s"), "{}", reqs[0]);
        assert!(reqs[0].contains("passing=true"), "{}", reqs[0]);
        assert!(reqs[0].contains("tag=eu"), "{}", reqs[0]);
        assert_eq!(src.index(), Some(6), "the new index is the next position");
        // The plain fetch carries no index.
        assert!(!server.requests()[0].contains("index="));
    }

    #[tokio::test]
    async fn consul_reissues_the_query_when_its_wait_runs_out() {
        // Two answers at the unchanged index (Consul's wait elapsed), then a change.
        let server = mock_http::serve_consul(
            Some(5),
            CONSUL_BODY,
            vec![(1100, Some(5)), (1100, Some(5)), (0, Some(7))],
        )
        .await;
        let src = blocking_consul(server.addr);
        src.fetch().await.unwrap();

        tokio::time::timeout(Duration::from_secs(10), src.changed())
            .await
            .expect("the change after two quiet waits is signalled");
        let reqs = server.blocking_requests();
        assert_eq!(reqs.len(), 3, "{reqs:?}");
        assert!(reqs.iter().all(|r| r.contains("index=5")), "{reqs:?}");
        assert_eq!(src.index(), Some(7));
    }

    #[tokio::test]
    async fn consul_backs_off_when_the_wait_is_ignored() {
        // Answers at once with the unchanged index, as a server ignoring `wait` would.
        let server = mock_http::serve_consul(
            Some(5),
            CONSUL_BODY,
            vec![(0, Some(5)), (0, Some(5)), (0, Some(7))],
        )
        .await;
        let src = blocking_consul(server.addr);
        src.fetch().await.unwrap();

        assert!(consul_pending(&src).await, "no change to report");
        assert_eq!(
            server.blocking_requests().len(),
            1,
            "the next query is paced, not immediate"
        );
    }

    #[tokio::test]
    async fn consul_asks_for_a_fetch_when_the_index_goes_backwards() {
        let server = mock_http::serve_consul(Some(9), CONSUL_BODY, vec![(0, Some(3))]).await;
        let src = blocking_consul(server.addr);
        src.fetch().await.unwrap();

        tokio::time::timeout(Duration::from_secs(2), src.changed())
            .await
            .expect("a reset index asks for a fetch");
        assert_eq!(src.index(), None);

        // Until that fetch renews the index, no query is made.
        assert!(consul_pending(&src).await);
        assert_eq!(server.blocking_requests().len(), 1);
    }

    #[tokio::test]
    async fn consul_without_an_index_header_stays_on_polling() {
        let server = mock_http::serve_consul(None, CONSUL_BODY, vec![]).await;
        let src = blocking_consul(server.addr);
        src.fetch().await.unwrap();
        assert_eq!(src.index(), None);
        assert!(consul_pending(&src).await);
        assert!(server.blocking_requests().is_empty());
    }

    #[tokio::test]
    async fn consul_watch_failures_are_retried_not_signalled() {
        let server = mock_http::serve_consul_failing_watch(Some(5), CONSUL_BODY).await;
        let src = blocking_consul(server.addr);
        src.fetch().await.unwrap();
        assert!(consul_pending(&src).await, "an error is not a change");
        assert_eq!(src.index(), Some(5), "the position is kept");
        assert_eq!(server.blocking_requests().len(), 1, "retried after a delay");
    }

    /// A scratch file in the OS temp dir, removed on drop.
    struct TempFile(std::path::PathBuf);

    impl TempFile {
        fn new(name: &str, contents: &str) -> Self {
            let path = std::env::temp_dir().join(format!("gsp-{}-{name}", std::process::id()));
            std::fs::write(&path, contents).unwrap();
            Self(path)
        }
        fn write(&self, contents: &str) {
            std::fs::write(&self.0, contents).unwrap();
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn token_file_rereads_after_the_interval_and_keeps_the_last_token_on_error() {
        let f = TempFile::new("token-reread", "one\n");
        let cached = TokenFile::new(&f.0);
        assert_eq!(cached.get().as_deref(), Some("one"), "trimmed");
        f.write("two");
        assert_eq!(
            cached.get().as_deref(),
            Some("one"),
            "reused inside the interval"
        );

        let live = TokenFile::with_reread(&f.0, Duration::ZERO);
        assert_eq!(live.get().as_deref(), Some("two"));
        f.write("three");
        assert_eq!(
            live.get().as_deref(),
            Some("three"),
            "rotation is picked up"
        );

        // Mid-rotation the file can be briefly absent: keep serving the old token.
        std::fs::remove_file(&f.0).unwrap();
        assert_eq!(live.get().as_deref(), Some("three"));

        // Never readable, or empty: no token.
        assert_eq!(TokenFile::new("/nonexistent/gsp-token").get(), None);
        f.write("  \n");
        assert_eq!(TokenFile::with_reread(&f.0, Duration::ZERO).get(), None);
    }

    #[tokio::test]
    async fn consul_sends_the_acl_token_on_fetch_and_blocking_queries_and_follows_rotation() {
        let f = TempFile::new("consul-token", "s3cret");
        let server =
            mock_http::serve_consul(Some(5), CONSUL_BODY, vec![(0, Some(6)), (0, Some(7))]).await;
        let src = ConsulSource::new(
            "p".into(),
            "game".into(),
            &format!("http://{}", server.addr),
            None,
            Some(TokenFile::with_reread(&f.0, Duration::ZERO)),
            Duration::from_secs(3600),
        )
        .unwrap();
        src.fetch().await.unwrap();
        src.changed().await;
        f.write("rotated");
        src.changed().await;

        let heads = server.heads();
        assert_eq!(heads.len(), 3, "{heads:?}");
        let token = |h: &str| {
            h.lines()
                .find_map(|l| l.strip_prefix("x-consul-token: "))
                .map(str::to_string)
        };
        assert_eq!(token(&heads[0]).as_deref(), Some("s3cret"), "fetch");
        assert_eq!(
            token(&heads[1]).as_deref(),
            Some("s3cret"),
            "blocking query"
        );
        assert_eq!(
            token(&heads[2]).as_deref(),
            Some("rotated"),
            "after rotation"
        );
    }

    #[tokio::test]
    async fn consul_without_a_token_file_sends_no_token_header() {
        let server = mock_http::serve_consul(Some(5), CONSUL_BODY, vec![]).await;
        let src = blocking_consul(server.addr);
        src.fetch().await.unwrap();
        assert!(!server.heads()[0].to_lowercase().contains("x-consul-token"));
    }

    #[test]
    fn the_consul_token_header_is_marked_sensitive() {
        let f = TempFile::new("consul-sensitive", "s3cret");
        let src = ConsulSource::new(
            "p".into(),
            "game".into(),
            "http://127.0.0.1:1",
            None,
            Some(TokenFile::new(&f.0)),
            Duration::from_secs(1),
        )
        .unwrap();
        let req = src
            .health_request(&src.client, None)
            .unwrap()
            .build()
            .unwrap();
        let value = req.headers().get("x-consul-token").unwrap();
        assert!(value.is_sensitive());
        assert!(!format!("{req:?}").contains("s3cret"));
    }

    #[tokio::test]
    async fn consul_escapes_the_service_and_tag_in_the_request() {
        let server = mock_http::serve_consul(Some(5), CONSUL_BODY, vec![(0, Some(6))]).await;
        let src = ConsulSource::new(
            "p".into(),
            "game/eu west".into(),
            &format!("http://{}/", server.addr),
            Some("a&b=c#d e%".into()),
            None,
            Duration::from_secs(3600),
        )
        .unwrap();
        src.fetch().await.unwrap();
        src.changed().await;

        for req in server.requests() {
            assert!(
                req.starts_with(
                    "GET /v1/health/service/game%2Feu%20west?passing=true&tag=a%26b%3Dc%23d+e%25"
                ),
                "{req}"
            );
        }
        assert!(server.requests()[1].contains("&index=5&wait=300s"));
    }

    #[test]
    fn consul_rejects_an_address_that_is_not_a_url() {
        let err = ConsulSource::new(
            "p".into(),
            "g".into(),
            "not a url",
            None,
            None,
            Duration::from_secs(1),
        )
        .err()
        .expect("an invalid address is refused at build time");
        assert!(err.to_string().contains("not a url"), "{err}");
    }

    #[tokio::test]
    async fn kubernetes_follows_a_rotated_service_account_token() {
        let f = TempFile::new("kube-token", "old-token");
        let server = mock_http::serve_k8s(LIST_V10, vec![event("MODIFIED", "11")]).await;
        let src = KubernetesSource::new(
            "p".into(),
            "games".into(),
            "match".into(),
            None,
            format!("http://{}", server.addr),
            Duration::from_secs(3600),
            KubeAuth {
                token: Some(TokenFile::with_reread(&f.0, Duration::ZERO)),
                ca_pem: None,
            },
        )
        .unwrap();

        src.fetch().await.unwrap();
        f.write("new-token");
        src.fetch().await.unwrap();
        src.changed().await; // the watch request

        let bearer = |h: &str| {
            h.lines()
                .find_map(|l| l.strip_prefix("authorization: Bearer "))
                .map(str::to_string)
        };
        let heads = server.heads();
        assert_eq!(heads.len(), 3, "{heads:?}");
        assert_eq!(bearer(&heads[0]).as_deref(), Some("old-token"));
        assert_eq!(bearer(&heads[1]).as_deref(), Some("new-token"), "list");
        assert_eq!(bearer(&heads[2]).as_deref(), Some("new-token"), "watch");
    }

    #[cfg(feature = "tunnel")]
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

    #[cfg(feature = "tunnel")]
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

    #[cfg(feature = "tunnel")]
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

    #[cfg(feature = "tunnel")]
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

    #[cfg(feature = "tunnel")]
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

        pub struct ConsulServer {
            pub addr: SocketAddr,
            requests: Arc<Mutex<Vec<String>>>,
            heads: Arc<Mutex<Vec<String>>>,
        }

        impl ConsulServer {
            /// Full request head (line + headers) of every request so far.
            pub fn heads(&self) -> Vec<String> {
                self.heads.lock().unwrap().clone()
            }

            /// Request lines of every request so far.
            pub fn requests(&self) -> Vec<String> {
                self.requests.lock().unwrap().clone()
            }

            /// Request lines of every blocking query (`index=`) so far.
            pub fn blocking_requests(&self) -> Vec<String> {
                self.requests()
                    .into_iter()
                    .filter(|r| r.contains("index="))
                    .collect()
            }
        }

        /// A Consul stand-in: a plain `GET` answers `body` with
        /// `X-Consul-Index: index`; the nth blocking query (`index=`) waits
        /// `delay_ms`, then answers with the nth `(delay, index)`. Past the
        /// script it holds the connection like a long poll.
        pub async fn serve_consul(
            index: Option<u64>,
            body: &str,
            blocking: Vec<(u64, Option<u64>)>,
        ) -> ConsulServer {
            serve_consul_with(index, body, blocking, false).await
        }

        /// As [`serve_consul`], but every blocking query fails with a `500`.
        pub async fn serve_consul_failing_watch(index: Option<u64>, body: &str) -> ConsulServer {
            serve_consul_with(index, body, vec![], true).await
        }

        async fn serve_consul_with(
            index: Option<u64>,
            body: &str,
            blocking: Vec<(u64, Option<u64>)>,
            fail_watch: bool,
        ) -> ConsulServer {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let log = requests.clone();
            let heads = Arc::new(Mutex::new(Vec::new()));
            let head_log = heads.clone();
            let body = body.to_string();
            let script = Arc::new(Mutex::new(std::collections::VecDeque::from(blocking)));
            tokio::spawn(async move {
                loop {
                    let Ok((mut sock, _)) = listener.accept().await else {
                        return;
                    };
                    let (log, body, script) = (log.clone(), body.clone(), script.clone());
                    let head_log = head_log.clone();
                    tokio::spawn(async move {
                        let mut buf = [0u8; 4096];
                        let n = sock.read(&mut buf).await.unwrap_or(0);
                        let head = String::from_utf8_lossy(&buf[..n]).to_string();
                        let line = head.lines().next().unwrap_or_default().to_string();
                        log.lock().unwrap().push(line.clone());
                        head_log.lock().unwrap().push(head.clone());
                        let (status, index) = if line.contains("index=") {
                            if fail_watch {
                                ("500 Internal Server Error", None)
                            } else {
                                let next = script.lock().unwrap().pop_front();
                                match next {
                                    Some((delay, idx)) => {
                                        tokio::time::sleep(std::time::Duration::from_millis(delay))
                                            .await;
                                        ("200 OK", idx)
                                    }
                                    None => {
                                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                                        return;
                                    }
                                }
                            }
                        } else {
                            ("200 OK", index)
                        };
                        let header = index
                            .map(|i| format!("x-consul-index: {i}\r\n"))
                            .unwrap_or_default();
                        let resp = format!(
                            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\n{header}\
                             content-length: {}\r\nconnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        let _ = sock.write_all(resp.as_bytes()).await;
                        let _ = sock.shutdown().await;
                    });
                }
            });
            ConsulServer {
                addr,
                requests,
                heads,
            }
        }

        pub struct K8sServer {
            pub addr: SocketAddr,
            requests: Arc<Mutex<Vec<String>>>,
            heads: Arc<Mutex<Vec<String>>>,
        }

        impl K8sServer {
            /// Full request head (line + headers) of every request so far.
            pub fn heads(&self) -> Vec<String> {
                self.heads.lock().unwrap().clone()
            }

            /// Request lines of every request so far.
            pub fn requests(&self) -> Vec<String> {
                self.requests.lock().unwrap().clone()
            }

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
            let heads = Arc::new(Mutex::new(Vec::new()));
            let head_log = heads.clone();
            let list = list.to_string();
            tokio::spawn(async move {
                loop {
                    let Ok((mut sock, _)) = listener.accept().await else {
                        return;
                    };
                    let (log, list, events) = (log.clone(), list.clone(), events.clone());
                    let head_log = head_log.clone();
                    tokio::spawn(async move {
                        let mut buf = [0u8; 4096];
                        let n = sock.read(&mut buf).await.unwrap_or(0);
                        let head = String::from_utf8_lossy(&buf[..n]).to_string();
                        let line = head.lines().next().unwrap_or_default().to_string();
                        log.lock().unwrap().push(line.clone());
                        head_log.lock().unwrap().push(head.clone());
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
            K8sServer {
                addr,
                requests,
                heads,
            }
        }
    }
}
