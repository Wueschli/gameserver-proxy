//! Concrete backend-discovery adapters (phase 8).
//!
//! `gsp-core` owns the [`BackendSource`] seam and the level-triggered
//! reconcile; the network clients live here in the binary, the same split as
//! resolvers. Each adapter answers one question — "what is the current backend
//! address set for this pool?" — and `gsp-core` diffs it against the live set.
//!
//! - [`DnsSrvSource`] resolves an SRV record (port from the record).
//! - [`ConsulSource`] lists passing instances of a Consul service.
//! - [`KubernetesSource`] polls the Endpoints of a Kubernetes service
//!   (a watch-based informer is a deferred optimisation — see `docs/08`).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context};
use async_trait::async_trait;
use gsp_config::{Config, SourceConfig, SourceKind};
use gsp_core::{BackendSource, SourceFactory};
use hickory_resolver::config::ResolverConfig;
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use hickory_resolver::TokioResolver;
use serde::Deserialize;

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
    fn build(&self, pool: &str, cfg: &SourceConfig) -> anyhow::Result<Arc<dyn BackendSource>> {
        build_one(pool, cfg, &self.kube_auth, self.tunnel_registry.as_ref())
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
            .and_then(|b| b.build())
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

    async fn fetch(&self) -> anyhow::Result<Vec<SocketAddr>> {
        let lookup = self
            .resolver
            .srv_lookup(&self.record)
            .await
            .with_context(|| format!("SRV lookup for {}", self.record))?;

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
                    let ips = self
                        .resolver
                        .lookup_ip(target)
                        .await
                        .with_context(|| format!("A/AAAA lookup for SRV target {target}"))?;
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

    async fn fetch(&self) -> anyhow::Result<Vec<SocketAddr>> {
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
            .map_err(|e| anyhow::anyhow!("Consul request: {}", gsp_http::error_chain(&e)))?
            .error_for_status()
            .map_err(|e| anyhow::anyhow!("Consul response status: {}", gsp_http::error_chain(&e)))?
            .json()
            .await
            .map_err(|e| {
                anyhow::anyhow!("decode Consul response: {}", gsp_http::error_chain(&e))
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
// Kubernetes Endpoints (polling)
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
}

#[derive(Deserialize)]
struct Endpoints {
    #[serde(default)]
    subsets: Vec<Subset>,
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
    pub fn new(
        pool: String,
        namespace: String,
        service: String,
        port_name: Option<String>,
        api: String,
        interval: Duration,
        auth: KubeAuth,
    ) -> anyhow::Result<Self> {
        let mut builder = gsp_http::builder().timeout(Duration::from_secs(5));
        if let Some(ca) = &auth.ca_pem {
            let cert = reqwest::Certificate::from_pem(ca).context("parse Kubernetes CA cert")?;
            builder = builder.add_root_certificate(cert);
        }
        Ok(Self {
            pool,
            namespace,
            service,
            port_name,
            api: api.trim_end_matches('/').to_string(),
            interval,
            token: auth.token,
            client: builder.build().context("build Kubernetes HTTP client")?,
        })
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

    async fn fetch(&self) -> anyhow::Result<Vec<SocketAddr>> {
        let url = format!(
            "{}/api/v1/namespaces/{}/endpoints/{}",
            self.api, self.namespace, self.service
        );
        let mut req = self.client.get(&url);
        if let Some(token) = &self.token {
            req = req.bearer_auth(token);
        }
        let ep: Endpoints = req
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Kubernetes request: {}", gsp_http::error_chain(&e)))?
            .error_for_status()
            .map_err(|e| {
                anyhow::anyhow!("Kubernetes response status: {}", gsp_http::error_chain(&e))
            })?
            .json()
            .await
            .map_err(|e| {
                anyhow::anyhow!("decode Kubernetes Endpoints: {}", gsp_http::error_chain(&e))
            })?;

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
                let ip = addr
                    .ip
                    .parse()
                    .map_err(|_| anyhow!("endpoint address {:?} is not an IP", addr.ip))?;
                out.push(SocketAddr::new(ip, port.port));
            }
        }
        Ok(out)
    }
}

/// Parse `host` as an IP, or resolve it (A/AAAA) and pair every result with
/// `port`.
async fn resolve_host_port(host: &str, port: u16) -> anyhow::Result<Vec<SocketAddr>> {
    if let Ok(ip) = host.parse() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .with_context(|| format!("resolve {host}:{port}"))?
        .collect();
    if addrs.is_empty() {
        return Err(anyhow!("{host} resolved to no addresses"));
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

    async fn fetch(&self) -> anyhow::Result<Vec<SocketAddr>> {
        let url = format!(
            "{}/peers/{}",
            self.controller_url.trim_end_matches('/'),
            self.origin
        );
        let mut req = self.client.get(&url);
        if let Some(token) = &self.token {
            req = req.bearer_auth(token);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("fetching {url}: {}", gsp_http::error_chain(&e)))?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            // The origin hasn't registered yet (or ever) — not an error,
            // just "no addresses known right now". The level-triggered
            // discovery contract already treats an empty `Ok` as "keep the
            // last-known-good set", so this needs no special handling here.
            return Ok(Vec::new());
        }
        if !resp.status().is_success() {
            anyhow::bail!("controller {url} returned {}", resp.status());
        }

        let reg: PeerRegistration = resp.json().await.map_err(|e| {
            anyhow::anyhow!(
                "parsing peer registration from {url}: {}",
                gsp_http::error_chain(&e)
            )
        })?;
        if reg.pubkey != self.pubkey {
            anyhow::bail!(
                "origin {:?} is currently registered with a different pubkey than \
                 backend_sources pins (expected {:?}, got {:?}) — refusing to trust it",
                self.origin,
                self.pubkey,
                reg.pubkey
            );
        }

        let mut out = Vec::with_capacity(reg.backends.len());
        for b in &reg.backends {
            out.push(b.parse().with_context(|| {
                format!(
                    "origin {:?} backend {b:?} is not a valid ip:port",
                    self.origin
                )
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
    fn assert_names_the_cause(e: anyhow::Error) {
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
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let body = body.to_string();
            let status_line = status_line.to_string();
            tokio::spawn(async move {
                loop {
                    let Ok((mut sock, _)) = listener.accept().await else {
                        return;
                    };
                    let body = body.clone();
                    let status_line = status_line.clone();
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
    }
}
