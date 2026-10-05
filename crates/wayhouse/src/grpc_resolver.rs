//! The gRPC external-resolver client (`resolvers[].kind: grpc`), behind the
//! `grpc-resolver` cargo feature: it is the only user of `tonic`/`prost` and of
//! `protoc` at build time.

use async_trait::async_trait;
use wayhouse_config::{OnError, ProxyProtocol, ResolverConfig};
use wayhouse_core::{Resolution, ResolveError, ResolveRequest, Resolver};

/// Generated from `proto/resolver.proto`.
#[allow(clippy::result_large_err)] // tonic's `Status` is large; generated code
#[allow(clippy::default_trait_access)] // generated code
mod pb {
    tonic::include_proto!("wayhouse.resolver.v1");
}

pub struct GrpcResolver {
    name: String,
    on_error: OnError,
    proxy_protocol: ProxyProtocol,
    target_connect_timeout: std::time::Duration,
    target_idle_timeout: std::time::Duration,
    channel: tonic::transport::Channel,
}

impl GrpcResolver {
    pub fn new(cfg: &ResolverConfig) -> anyhow::Result<Self> {
        let channel = tonic::transport::Endpoint::from_shared(cfg.endpoint.clone())
            .map_err(|e| anyhow::anyhow!("resolver {}: bad grpc endpoint: {e}", cfg.name))?
            .timeout(cfg.timeout)
            .connect_lazy();
        Ok(Self {
            name: cfg.name.clone(),
            on_error: cfg.on_error,
            proxy_protocol: cfg.proxy_protocol,
            target_connect_timeout: cfg.target_connect_timeout,
            target_idle_timeout: cfg.target_idle_timeout,
            channel,
        })
    }
}

fn opt(s: String) -> Option<String> {
    (!s.is_empty()).then_some(s)
}

#[async_trait]
impl Resolver for GrpcResolver {
    fn name(&self) -> &str {
        &self.name
    }
    fn on_error(&self) -> OnError {
        self.on_error
    }
    fn proxy_protocol(&self) -> ProxyProtocol {
        self.proxy_protocol
    }
    fn target_connect_timeout(&self) -> std::time::Duration {
        self.target_connect_timeout
    }
    fn target_idle_timeout(&self) -> std::time::Duration {
        self.target_idle_timeout
    }
    async fn resolve(&self, req: ResolveRequest) -> Result<Resolution, ResolveError> {
        let mut client = pb::resolver_client::ResolverClient::new(self.channel.clone());
        let pb_req = pb::ResolveRequest {
            listener: req.listener,
            src: req.src.to_string(),
            dst: req.dst.to_string(),
            sni: req.sni.unwrap_or_default(),
            first_bytes: req.first_bytes,
            routing_key: req.routing_key.unwrap_or_default(),
        };
        let resp = client.resolve(pb_req).await.map_err(|s| match s.code() {
            tonic::Code::DeadlineExceeded => ResolveError::Timeout,
            _ => ResolveError::Failed(s.message().to_string()),
        })?;
        let r = resp.into_inner();
        let target = match opt(r.target) {
            Some(s) => Some(
                s.parse()
                    .map_err(|_| ResolveError::Failed(format!("bad target address {s:?}")))?,
            ),
            None => None,
        };
        Ok(Resolution {
            pool: opt(r.pool),
            target,
            sticky_key: opt(r.sticky_key),
            ttl_sec: (r.ttl_sec != 0).then_some(r.ttl_sec as u64),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tonic `Resolver` server that echoes the request's SNI into the pool
    /// name, so the round trip is observable.
    struct EchoSvc;
    #[tonic::async_trait]
    impl pb::resolver_server::Resolver for EchoSvc {
        async fn resolve(
            &self,
            request: tonic::Request<pb::ResolveRequest>,
        ) -> Result<tonic::Response<pb::Resolution>, tonic::Status> {
            let r = request.into_inner();
            Ok(tonic::Response::new(pb::Resolution {
                pool: format!(
                    "pool-for-{}",
                    if r.sni.is_empty() { "none" } else { &r.sni }
                ),
                ttl_sec: 10,
                ..Default::default()
            }))
        }
    }

    #[tokio::test]
    async fn grpc_resolver_round_trips_a_request() {
        // A free port, then hand it to the tonic server.
        let addr: std::net::SocketAddr = {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            l.local_addr().unwrap()
        };
        let server = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(pb::resolver_server::ResolverServer::new(EchoSvc))
                .serve(addr)
                .await
                .unwrap();
        });
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        let rc = wayhouse_config::ResolverConfig {
            name: "mm".into(),
            kind: wayhouse_config::ResolverKind::Grpc,
            endpoint: format!("http://{addr}"),
            timeout: std::time::Duration::from_secs(2),
            on_error: wayhouse_config::OnError::Reject,
            cache: None,
            proxy_protocol: wayhouse_config::ProxyProtocol::None,
            target_connect_timeout: std::time::Duration::from_millis(300),
            target_idle_timeout: std::time::Duration::from_secs(90),
        };
        let r = GrpcResolver::new(&rc).unwrap();
        assert_eq!(r.name(), "mm");

        let res = r
            .resolve(ResolveRequest {
                listener: "l".into(),
                src: "1.2.3.4:5".parse().unwrap(),
                dst: "9.9.9.9:7".parse().unwrap(),
                sni: Some("eu.example.com".into()),
                first_bytes: vec![],
                routing_key: None,
            })
            .await
            .unwrap();
        assert_eq!(res.pool.as_deref(), Some("pool-for-eu.example.com"));
        assert_eq!(res.ttl_sec, Some(10));

        server.abort();
    }
}
