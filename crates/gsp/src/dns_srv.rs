//! The DNS SRV backend-discovery adapter (`backend_sources[].type: dns_srv`),
//! behind the `dns-srv` cargo feature: it is the only user of `hickory-resolver`.

use std::net::SocketAddr;
use std::time::Duration;

use async_trait::async_trait;
use gsp_core::{BackendSource, SourceError};
use hickory_resolver::config::ResolverConfig;
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use hickory_resolver::TokioResolver;

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
