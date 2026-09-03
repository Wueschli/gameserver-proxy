//! External routing resolvers (phase 4).
//!
//! A route `action: { resolver: <name> }` hands the routing decision to an
//! out-of-process service. `gsp-core` owns the **contract** (this module) and
//! the routing integration ([`resolve_pool`]); the HTTP / gRPC clients that
//! actually speak to the service live in the `gsp` binary (keeps `gsp-core`
//! HTTP-free) and are injected at startup as `Arc<dyn Resolver>`.
//!
//! Slice 1: `pool` results only, `on_error` = `reject` / `fallback_route`
//! (`stale_ok` and `target` / `sticky_key` / the cache come in later slices).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use gsp_config::{Action, ListenerConfig, MatchContext, OnError};

use crate::metrics_defs as m;

/// What the proxy tells a resolver about a pending connection / session.
#[derive(Debug, Clone)]
pub struct ResolveRequest {
    pub listener: String,
    pub src: SocketAddr,
    pub dst: SocketAddr,
    pub sni: Option<String>,
    pub first_bytes: Vec<u8>,
    pub routing_key: Option<String>,
}

/// A resolver's answer.
#[derive(Debug, Clone, Default)]
pub struct Resolution {
    /// Route to this pool (normal LB). Slice 1 uses only this.
    pub pool: Option<String>,
    /// Route straight to this instance, bypassing pools (slice 4).
    pub target: Option<SocketAddr>,
    /// Affinity key to remember the decision by (slice 4).
    pub sticky_key: Option<String>,
    /// Positive cache TTL from the service (slice 2).
    pub ttl_sec: Option<u64>,
}

#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("resolver timed out")]
    Timeout,
    #[error("resolver call failed: {0}")]
    Failed(String),
}

#[async_trait]
pub trait Resolver: Send + Sync {
    fn name(&self) -> &str;
    fn on_error(&self) -> OnError;
    async fn resolve(&self, req: ResolveRequest) -> Result<Resolution, ResolveError>;
}

/// Name → resolver, built once from config and shared across listeners.
pub type Resolvers = HashMap<String, Arc<dyn Resolver>>;

/// Walk the listener's matching routes and return the chosen pool name, or
/// `None` to drop. `Pool` actions end the walk; a `Resolver` action calls the
/// service and, on failure, either drops (`reject` / `stale_ok` w/o a cache) or
/// continues to the next matching route (`fallback_route`).
pub async fn resolve_pool(
    cfg: &ListenerConfig,
    resolvers: &Resolvers,
    mctx: &MatchContext<'_>,
    first: &[u8],
) -> Option<String> {
    // Collect the matching actions first so we do not hold the route iterator
    // (which borrows `mctx`) across an `.await`.
    let actions: Vec<Action> = cfg
        .matching_routes(mctx)
        .map(|r| r.action.clone())
        .collect();

    for action in actions {
        match action {
            Action::Pool(p) => return Some(p),
            Action::Resolver(name) => {
                let Some(resolver) = resolvers.get(&name) else {
                    // validate() guarantees the name exists; be defensive.
                    tracing::error!(resolver = %name, "resolver missing from the registry");
                    return None;
                };
                let req = ResolveRequest {
                    listener: cfg.name.clone(),
                    src: mctx.src,
                    dst: mctx.local,
                    sni: gsp_config::extract_sni(first),
                    first_bytes: first.to_vec(),
                    routing_key: mctx.sniff.and_then(|h| h.key.clone()),
                };
                match resolver.resolve(req).await {
                    Ok(res) if res.pool.is_some() => {
                        metrics::counter!(
                            m::RESOLVER_REQUESTS, "resolver" => name.clone(), "result" => "ok",
                        )
                        .increment(1);
                        return res.pool;
                    }
                    Ok(_) => {
                        // Recognised nothing (empty resolution). Treat like an
                        // error for `on_error` purposes.
                        metrics::counter!(
                            m::RESOLVER_REQUESTS, "resolver" => name.clone(), "result" => "empty",
                        )
                        .increment(1);
                        if !on_error_continues(resolver.on_error()) {
                            return None;
                        }
                    }
                    Err(e) => {
                        let result = match e {
                            ResolveError::Timeout => "timeout",
                            ResolveError::Failed(_) => "error",
                        };
                        metrics::counter!(
                            m::RESOLVER_REQUESTS, "resolver" => name.clone(), "result" => result,
                        )
                        .increment(1);
                        tracing::warn!(resolver = %name, error = %e, "resolver call failed");
                        if !on_error_continues(resolver.on_error()) {
                            return None;
                        }
                    }
                }
            }
        }
    }
    None
}

/// `fallback_route` → keep walking the route list; `reject` / `stale_ok` (no
/// cache yet) → stop and drop.
fn on_error_continues(on_error: OnError) -> bool {
    matches!(on_error, OnError::FallbackRoute)
}

#[cfg(test)]
mod tests {
    use super::*;

    enum Mode {
        Pool(&'static str),
        Empty,
        Err,
    }
    struct Stub {
        mode: Mode,
        on_error: OnError,
    }
    #[async_trait]
    impl Resolver for Stub {
        fn name(&self) -> &str {
            "stub"
        }
        fn on_error(&self) -> OnError {
            self.on_error
        }
        async fn resolve(&self, _req: ResolveRequest) -> Result<Resolution, ResolveError> {
            match self.mode {
                Mode::Pool(p) => Ok(Resolution {
                    pool: Some(p.into()),
                    ..Default::default()
                }),
                Mode::Empty => Ok(Resolution::default()),
                Mode::Err => Err(ResolveError::Failed("boom".into())),
            }
        }
    }

    fn listener_with_resolver() -> ListenerConfig {
        let cfg = gsp_config::parse_str(
            r#"
pools:
  - { name: fallback, targets: ["127.0.0.1:1"] }
resolvers:
  - { name: mm, endpoint: "http://x" }
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    routes:
      - { match: { type: always }, action: { resolver: mm } }
      - { match: { type: always }, action: { pool: fallback } }
"#,
        )
        .unwrap();
        cfg.listeners.into_iter().next().unwrap()
    }

    async fn run(mode: Mode, on_error: OnError) -> Option<String> {
        let lc = listener_with_resolver();
        let mut resolvers = Resolvers::new();
        resolvers.insert(
            "mm".to_string(),
            Arc::new(Stub { mode, on_error }) as Arc<dyn Resolver>,
        );
        let mctx = MatchContext {
            src: "9.9.9.9:1".parse().unwrap(),
            local: "1.1.1.1:7777".parse().unwrap(),
            first_bytes: &[],
            sniff: None,
        };
        resolve_pool(&lc, &resolvers, &mctx, &[]).await
    }

    #[tokio::test]
    async fn resolver_pool_result_is_used() {
        assert_eq!(
            run(Mode::Pool("prod"), OnError::Reject).await.as_deref(),
            Some("prod")
        );
    }

    #[tokio::test]
    async fn on_error_reject_drops() {
        assert_eq!(run(Mode::Err, OnError::Reject).await, None);
        assert_eq!(run(Mode::Empty, OnError::Reject).await, None);
        // stale_ok has no cache yet -> behaves like reject
        assert_eq!(run(Mode::Err, OnError::StaleOk).await, None);
    }

    #[tokio::test]
    async fn on_error_fallback_route_continues_to_next_match() {
        assert_eq!(
            run(Mode::Err, OnError::FallbackRoute).await.as_deref(),
            Some("fallback")
        );
        assert_eq!(
            run(Mode::Empty, OnError::FallbackRoute).await.as_deref(),
            Some("fallback")
        );
    }

    /// End to end: a resolver `action` on a live listener routes a real
    /// connection to the pool the (stub) resolver names.
    #[tokio::test]
    async fn resolver_action_routes_a_live_connection() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::{TcpListener, TcpStream};

        async fn marker(tag: u8) -> std::net::SocketAddr {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = l.local_addr().unwrap();
            tokio::spawn(async move {
                while let Ok((mut s, _)) = l.accept().await {
                    tokio::spawn(async move {
                        let _ = s.write_all(&[tag]).await;
                        let mut b = [0u8; 32];
                        while let Ok(n) = s.read(&mut b).await {
                            if n == 0 {
                                break;
                            }
                        }
                    });
                }
            });
            addr
        }

        let mm_pool = marker(b'M').await;
        let fallback = marker(b'F').await;
        let proxy = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap();

        let yaml = format!(
            r#"
pools:
  - {{ name: mm-pool, targets: ["{mm_pool}"] }}
  - {{ name: fallback, targets: ["{fallback}"] }}
resolvers:
  - {{ name: mm, endpoint: "http://unused" }}
listeners:
  - name: l
    bind: "{proxy}"
    routes:
      - {{ match: {{ type: always }}, action: {{ resolver: mm }} }}
      - {{ match: {{ type: always }}, action: {{ pool: fallback }} }}
"#
        );
        let cfg = gsp_config::parse_str(&yaml).unwrap();
        let mut resolvers = Resolvers::new();
        resolvers.insert(
            "mm".to_string(),
            Arc::new(Stub {
                mode: Mode::Pool("mm-pool"),
                on_error: OnError::Reject,
            }) as Arc<dyn Resolver>,
        );
        let runtime =
            crate::Runtime::start(crate::Snapshot::from_config(&cfg), Arc::new(resolvers), 1);
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        let mut c = TcpStream::connect(proxy).await.unwrap();
        let mut m = [0u8; 1];
        c.read_exact(&mut m).await.unwrap();
        assert_eq!(m[0], b'M', "resolver named mm-pool");

        runtime.shutdown().await;
    }
}
