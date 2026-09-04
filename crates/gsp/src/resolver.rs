//! Concrete external-resolver clients. The trait + routing integration live in
//! `gsp-core`; the transports (HTTP now, gRPC in a later slice) live here so
//! `gsp-core` stays free of an HTTP client.

use std::sync::Arc;

use async_trait::async_trait;
use gsp_config::{Config, OnError, ProxyProtocol, ResolverConfig, ResolverKind};
use gsp_core::{CachedResolver, Resolution, ResolveError, ResolveRequest, Resolver, Resolvers};
use serde::{Deserialize, Serialize};

/// Generated from `proto/resolver.proto`.
#[allow(clippy::result_large_err)] // tonic's `Status` is large; generated code
mod pb {
    tonic::include_proto!("gsp.resolver.v1");
}

/// Build the name → resolver map from config. Errors if a resolver cannot be
/// constructed (bad endpoint URL, unimplemented transport).
pub fn build_resolvers(cfg: &Config) -> anyhow::Result<Resolvers> {
    let mut map = Resolvers::new();
    for rc in &cfg.resolvers {
        let inner: Arc<dyn Resolver> = match rc.kind {
            ResolverKind::Http => Arc::new(HttpResolver::new(rc)?),
            ResolverKind::Grpc => Arc::new(GrpcResolver::new(rc)?),
        };
        let r = match &rc.cache {
            Some(cc) => Arc::new(CachedResolver::new(inner, cc)) as Arc<dyn Resolver>,
            None => inner,
        };
        map.insert(rc.name.clone(), r);
    }
    Ok(map)
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct WireRequest<'a> {
    listener: &'a str,
    src: String,
    dst: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    sni: Option<&'a str>,
    first_bytes_b64: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    routing_key: Option<&'a str>,
}

#[derive(Deserialize, Default)]
struct WireResponse {
    #[serde(default)]
    pool: Option<String>,
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    sticky_key: Option<String>,
    #[serde(default)]
    ttl_sec: Option<u64>,
}

pub struct HttpResolver {
    name: String,
    endpoint: String,
    on_error: OnError,
    proxy_protocol: ProxyProtocol,
    target_connect_timeout: std::time::Duration,
    target_idle_timeout: std::time::Duration,
    client: reqwest::Client,
}

impl HttpResolver {
    fn new(cfg: &ResolverConfig) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(cfg.timeout)
            .build()
            .map_err(|e| anyhow::anyhow!("resolver {}: {e}", cfg.name))?;
        Ok(Self {
            name: cfg.name.clone(),
            endpoint: cfg.endpoint.clone(),
            on_error: cfg.on_error,
            proxy_protocol: cfg.proxy_protocol,
            target_connect_timeout: cfg.target_connect_timeout,
            target_idle_timeout: cfg.target_idle_timeout,
            client,
        })
    }
}

#[async_trait]
impl Resolver for HttpResolver {
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
        let body = WireRequest {
            listener: &req.listener,
            src: req.src.to_string(),
            dst: req.dst.to_string(),
            sni: req.sni.as_deref(),
            first_bytes_b64: base64_encode(&req.first_bytes),
            routing_key: req.routing_key.as_deref(),
        };
        let resp = self
            .client
            .post(&self.endpoint)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    ResolveError::Timeout
                } else {
                    ResolveError::Failed(e.to_string())
                }
            })?;
        if !resp.status().is_success() {
            return Err(ResolveError::Failed(format!("HTTP {}", resp.status())));
        }
        let w: WireResponse = resp
            .json()
            .await
            .map_err(|e| ResolveError::Failed(format!("bad response body: {e}")))?;
        let target = match w.target.as_deref() {
            Some(s) => Some(
                s.parse()
                    .map_err(|_| ResolveError::Failed(format!("bad target address {s:?}")))?,
            ),
            None => None,
        };
        Ok(Resolution {
            pool: w.pool,
            target,
            sticky_key: w.sticky_key,
            ttl_sec: w.ttl_sec,
        })
    }
}

// ---------------------------------------------------------------------------
// gRPC
// ---------------------------------------------------------------------------

pub struct GrpcResolver {
    name: String,
    on_error: OnError,
    proxy_protocol: ProxyProtocol,
    target_connect_timeout: std::time::Duration,
    target_idle_timeout: std::time::Duration,
    channel: tonic::transport::Channel,
}

impl GrpcResolver {
    fn new(cfg: &ResolverConfig) -> anyhow::Result<Self> {
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

/// Standard base64 (with `=` padding). Kept local to avoid another dependency.
fn base64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(&[0xff, 0xff, 0xff, 0xff]), "/////w==");
    }

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

        let rc = gsp_config::ResolverConfig {
            name: "mm".into(),
            kind: gsp_config::ResolverKind::Grpc,
            endpoint: format!("http://{addr}"),
            timeout: std::time::Duration::from_secs(2),
            on_error: gsp_config::OnError::Reject,
            cache: None,
            proxy_protocol: gsp_config::ProxyProtocol::None,
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
