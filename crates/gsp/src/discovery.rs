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
use hickory_resolver::config::{ResolverConfig, ResolverOpts};
use hickory_resolver::TokioAsyncResolver;
use serde::Deserialize;

/// In-pod service-account paths for the Kubernetes API.
const K8S_TOKEN_PATH: &str = "/var/run/secrets/kubernetes.io/serviceaccount/token";
const K8S_CA_PATH: &str = "/var/run/secrets/kubernetes.io/serviceaccount/ca.crt";

/// Build one [`BackendSource`] per pool that declares a dynamic `source`.
/// Used at startup for the best-effort initial fetch; the live [`SourceManager`]
/// rebuilds sources through [`DiscoveryFactory`] on reload.
pub fn build_sources(cfg: &Config) -> anyhow::Result<Vec<Arc<dyn BackendSource>>> {
    let kube_auth = KubeAuth::from_pod();
    let mut out: Vec<Arc<dyn BackendSource>> = Vec::new();
    for p in &cfg.pools {
        let Some(sc) = &p.source else { continue };
        out.push(build_one(&p.name, sc, &kube_auth)?);
    }
    Ok(out)
}

/// Build the concrete adapter for one pool's `source`.
pub fn build_one(
    pool: &str,
    sc: &SourceConfig,
    kube_auth: &KubeAuth,
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
    };
    Ok(src)
}

/// [`SourceFactory`] for the live [`SourceManager`]: rebuilds a pool's adapter
/// from its (possibly changed) [`SourceConfig`] on reload. Holds the in-pod
/// Kubernetes credentials read once at startup.
pub struct DiscoveryFactory {
    kube_auth: KubeAuth,
}

impl DiscoveryFactory {
    pub fn new() -> Self {
        Self {
            kube_auth: KubeAuth::from_pod(),
        }
    }
}

impl SourceFactory for DiscoveryFactory {
    fn build(&self, pool: &str, cfg: &SourceConfig) -> anyhow::Result<Arc<dyn BackendSource>> {
        build_one(pool, cfg, &self.kube_auth)
    }
}

// ---------------------------------------------------------------------------
// DNS SRV
// ---------------------------------------------------------------------------

pub struct DnsSrvSource {
    pool: String,
    record: String,
    interval: Duration,
    resolver: TokioAsyncResolver,
}

impl DnsSrvSource {
    pub fn new(pool: String, record: String, interval: Duration) -> anyhow::Result<Self> {
        // Prefer the host resolver config; fall back to a default (public) one
        // so a missing /etc/resolv.conf doesn't abort startup.
        let resolver = TokioAsyncResolver::tokio_from_system_conf().unwrap_or_else(|e| {
            tracing::warn!(error = %e, "dns_srv: system resolver config unavailable; using defaults");
            TokioAsyncResolver::tokio(ResolverConfig::default(), ResolverOpts::default())
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
        use hickory_resolver::config::{NameServerConfigGroup, ResolverConfig};
        let group =
            NameServerConfigGroup::from_ips_clear(&[nameserver.ip()], nameserver.port(), true);
        let cfg = ResolverConfig::from_parts(None, vec![], group);
        Self {
            pool,
            record,
            interval,
            resolver: TokioAsyncResolver::tokio(cfg, ResolverOpts::default()),
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
        for srv in lookup.iter() {
            let port = srv.port();
            let target = srv.target().to_utf8();
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
            client: reqwest::Client::builder()
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
            .context("Consul request")?
            .error_for_status()
            .context("Consul response status")?
            .json()
            .await
            .context("decode Consul response")?;

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
        let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(5));
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
            .context("Kubernetes request")?
            .error_for_status()
            .context("Kubernetes response status")?
            .json()
            .await
            .context("decode Kubernetes Endpoints")?;

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

#[cfg(test)]
mod tests {
    use super::*;

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
        use hickory_server::authority::{Catalog, ZoneType};
        use hickory_server::proto::rr::rdata::{A, SRV};
        use hickory_server::proto::rr::{LowerName, Name, RData, Record};
        use hickory_server::store::in_memory::InMemoryAuthority;
        use hickory_server::ServerFuture;
        use std::sync::Arc;

        let origin = Name::from_ascii("example.com.").unwrap();
        let mut auth = InMemoryAuthority::empty(origin.clone(), ZoneType::Primary, false);
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
        catalog.upsert(LowerName::from(origin), Box::new(Arc::new(auth)));

        let udp = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let ns = udp.local_addr().unwrap();
        let mut server = ServerFuture::new(catalog);
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
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let body = body.to_string();
            tokio::spawn(async move {
                loop {
                    let Ok((mut sock, _)) = listener.accept().await else {
                        return;
                    };
                    let body = body.clone();
                    tokio::spawn(async move {
                        let mut buf = [0u8; 2048];
                        let _ = sock.read(&mut buf).await;
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
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
