//! Concrete external-resolver clients. The trait + routing integration live in
//! `gsp-core`; the transports (HTTP now, gRPC in a later slice) live here so
//! `gsp-core` stays free of an HTTP client.

use std::sync::Arc;

use async_trait::async_trait;
use gsp_config::{Config, OnError, ResolverConfig, ResolverKind};
use gsp_core::{Resolution, ResolveError, ResolveRequest, Resolver, Resolvers};
use serde::{Deserialize, Serialize};

/// Build the name → resolver map from config. Errors if a resolver cannot be
/// constructed (bad endpoint URL, unimplemented transport).
pub fn build_resolvers(cfg: &Config) -> anyhow::Result<Resolvers> {
    let mut map = Resolvers::new();
    for rc in &cfg.resolvers {
        let r: Arc<dyn Resolver> = match rc.kind {
            ResolverKind::Http => Arc::new(HttpResolver::new(rc)?),
            ResolverKind::Grpc => anyhow::bail!(
                "resolver {}: grpc transport is not implemented yet (phase 4, slice 3)",
                rc.name
            ),
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
    use super::base64_encode;

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(&[0xff, 0xff, 0xff, 0xff]), "/////w==");
    }
}
