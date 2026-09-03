//! External routing resolvers (phase 4).
//!
//! A route `action: { resolver: <name> }` hands the routing decision to an
//! out-of-process service. `gsp-core` owns the **contract** (this module) and
//! the routing integration ([`resolve_pool`]); the HTTP / gRPC clients that
//! actually speak to the service live in the `gsp` binary (keeps `gsp-core`
//! HTTP-free) and are injected at startup as `Arc<dyn Resolver>`.
//!
//! Done: HTTP + gRPC transports (in `gsp`), `pool` **and `target`** results,
//! `on_error` (`reject` / `fallback_route` / `stale_ok`), and the
//! [`CachedResolver`] TTL'd LRU cache. Pending: `Resolution::sticky_key` (a
//! resolver-chosen affinity key — overlaps the request-keyed cache, the
//! `route_hint` table and UDP affinity; needs its own design for how a later
//! request recovers the key without re-calling the resolver).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use gsp_config::{Action, CacheConfig, CacheKeyPart, ListenerConfig, MatchContext, OnError};
use lru::LruCache;

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
    /// Route to this pool (normal LB).
    pub pool: Option<String>,
    /// Or route straight to this instance, bypassing pools — no health check,
    /// no per-backend cap. Takes precedence over `pool`.
    pub target: Option<SocketAddr>,
    /// Affinity key to remember the decision by. **Not yet used** (see the
    /// module docs).
    pub sticky_key: Option<String>,
    /// Positive cache TTL hint (overrides the resolver's `positive_ttl_sec`).
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

/// The outcome of routing: a pool (normal LB) or a fixed instance (`target`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Routed {
    Pool(String),
    Target(SocketAddr),
}

/// Walk the listener's matching routes and return the route, or `None` to drop.
/// `Pool` actions end the walk; a `Resolver` action calls the service and, on
/// failure, either drops (`reject` / `stale_ok` w/o a stale entry) or continues
/// to the next matching route (`fallback_route`).
pub async fn resolve_route(
    cfg: &ListenerConfig,
    resolvers: &Resolvers,
    mctx: &MatchContext<'_>,
    first: &[u8],
) -> Option<Routed> {
    // Collect the matching actions first so we do not hold the route iterator
    // (which borrows `mctx`) across an `.await`.
    let actions: Vec<Action> = cfg
        .matching_routes(mctx)
        .map(|r| r.action.clone())
        .collect();

    for action in actions {
        match action {
            Action::Pool(p) => return Some(Routed::Pool(p)),
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
                    Ok(res) if res.target.is_some() || res.pool.is_some() => {
                        metrics::counter!(
                            m::RESOLVER_REQUESTS, "resolver" => name.clone(), "result" => "ok",
                        )
                        .increment(1);
                        return Some(match res.target {
                            Some(addr) => Routed::Target(addr),
                            None => Routed::Pool(res.pool.unwrap()),
                        });
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

/// `fallback_route` → keep walking the route list; `reject` / `stale_ok` (a
/// stale hit is served by [`CachedResolver`] before we get here, so reaching
/// this with `stale_ok` means "no stale entry") → stop and drop.
fn on_error_continues(on_error: OnError) -> bool {
    matches!(on_error, OnError::FallbackRoute)
}

// ---------------------------------------------------------------------------
// Result cache (phase 4 slice 2)
// ---------------------------------------------------------------------------

enum Entry {
    Positive { res: Resolution, expires: Instant },
    Negative { expires: Instant },
}

/// Wraps a [`Resolver`] with a TTL'd LRU result cache. The cache key is built
/// from the configured [`CacheKeyPart`]s; a request missing a required part
/// (e.g. `sni` with no SNI) is not cached and passes straight through.
pub struct CachedResolver {
    inner: Arc<dyn Resolver>,
    cache: Mutex<LruCache<String, Entry>>,
    key: Vec<CacheKeyPart>,
    positive_ttl: Duration,
    negative_ttl: Duration,
}

impl CachedResolver {
    pub fn new(inner: Arc<dyn Resolver>, cfg: &CacheConfig) -> Self {
        Self {
            inner,
            cache: Mutex::new(LruCache::new(
                NonZeroUsize::new(cfg.max_entries).unwrap_or(NonZeroUsize::MIN),
            )),
            key: cfg.key.clone(),
            positive_ttl: cfg.positive_ttl,
            negative_ttl: cfg.negative_ttl,
        }
    }

    fn key_for(&self, req: &ResolveRequest) -> Option<String> {
        let mut out = String::new();
        for (i, part) in self.key.iter().enumerate() {
            if i > 0 {
                out.push('|');
            }
            match part {
                CacheKeyPart::SrcIp => {
                    let _ = write!(out, "{}", req.src.ip());
                }
                CacheKeyPart::SrcIpPort => {
                    let _ = write!(out, "{}", req.src);
                }
                CacheKeyPart::Sni => out.push_str(req.sni.as_deref()?),
                CacheKeyPart::RoutingKey => out.push_str(req.routing_key.as_deref()?),
                CacheKeyPart::FirstBytes(r) => {
                    let end = (*r.end()).min(req.first_bytes.len());
                    let start = (*r.start()).min(end);
                    for b in &req.first_bytes[start..end] {
                        let _ = write!(out, "{b:02x}");
                    }
                }
            }
        }
        Some(out)
    }
}

#[async_trait]
impl Resolver for CachedResolver {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn on_error(&self) -> OnError {
        self.inner.on_error()
    }
    async fn resolve(&self, req: ResolveRequest) -> Result<Resolution, ResolveError> {
        let name = self.inner.name().to_string();
        let Some(key) = self.key_for(&req) else {
            metrics::counter!(
                m::RESOLVER_CACHE, "resolver" => name, "result" => "uncacheable",
            )
            .increment(1);
            return self.inner.resolve(req).await;
        };

        // Fresh cache hit?
        {
            let mut guard = self.cache.lock().unwrap();
            match guard.get(&key) {
                Some(Entry::Positive { res, expires }) if *expires > Instant::now() => {
                    let res = res.clone();
                    drop(guard);
                    metrics::counter!(
                        m::RESOLVER_CACHE, "resolver" => name, "result" => "hit",
                    )
                    .increment(1);
                    return Ok(res);
                }
                Some(Entry::Negative { expires }) if *expires > Instant::now() => {
                    drop(guard);
                    metrics::counter!(
                        m::RESOLVER_CACHE, "resolver" => name, "result" => "hit_negative",
                    )
                    .increment(1);
                    return Err(ResolveError::Failed("negatively cached".into()));
                }
                _ => {}
            }
        }

        metrics::counter!(
            m::RESOLVER_CACHE, "resolver" => name.clone(), "result" => "miss",
        )
        .increment(1);

        match self.inner.resolve(req).await {
            Ok(res) if res.pool.is_some() || res.target.is_some() => {
                let ttl = res
                    .ttl_sec
                    .map(Duration::from_secs)
                    .unwrap_or(self.positive_ttl);
                self.cache.lock().unwrap().put(
                    key,
                    Entry::Positive {
                        res: res.clone(),
                        expires: Instant::now() + ttl,
                    },
                );
                Ok(res)
            }
            Ok(empty) => {
                self.cache.lock().unwrap().put(
                    key,
                    Entry::Negative {
                        expires: Instant::now() + self.negative_ttl,
                    },
                );
                Ok(empty)
            }
            Err(e) => {
                if self.inner.on_error() == OnError::StaleOk {
                    let mut guard = self.cache.lock().unwrap();
                    if let Some(Entry::Positive { res, .. }) = guard.get(&key) {
                        let res = res.clone();
                        drop(guard);
                        metrics::counter!(
                            m::RESOLVER_CACHE, "resolver" => name, "result" => "stale",
                        )
                        .increment(1);
                        return Ok(res);
                    }
                }
                self.cache.lock().unwrap().put(
                    key,
                    Entry::Negative {
                        expires: Instant::now() + self.negative_ttl,
                    },
                );
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy)]
    enum Mode {
        Pool(&'static str),
        Target(&'static str),
        Empty,
        Err,
    }
    fn mode_result(mode: Mode) -> Result<Resolution, ResolveError> {
        match mode {
            Mode::Pool(p) => Ok(Resolution {
                pool: Some(p.into()),
                ..Default::default()
            }),
            Mode::Target(a) => Ok(Resolution {
                target: Some(a.parse().unwrap()),
                ..Default::default()
            }),
            Mode::Empty => Ok(Resolution::default()),
            Mode::Err => Err(ResolveError::Failed("boom".into())),
        }
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
            mode_result(self.mode)
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

    async fn run(mode: Mode, on_error: OnError) -> Option<Routed> {
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
        resolve_route(&lc, &resolvers, &mctx, &[]).await
    }

    #[tokio::test]
    async fn resolver_pool_result_is_used() {
        assert_eq!(
            run(Mode::Pool("prod"), OnError::Reject).await,
            Some(Routed::Pool("prod".into()))
        );
    }

    #[tokio::test]
    async fn resolver_target_result_is_used() {
        assert_eq!(
            run(Mode::Target("10.2.0.5:7777"), OnError::Reject).await,
            Some(Routed::Target("10.2.0.5:7777".parse().unwrap()))
        );
    }

    #[tokio::test]
    async fn on_error_reject_drops() {
        assert_eq!(run(Mode::Err, OnError::Reject).await, None);
        assert_eq!(run(Mode::Empty, OnError::Reject).await, None);
        // stale_ok with no stale entry -> behaves like reject
        assert_eq!(run(Mode::Err, OnError::StaleOk).await, None);
    }

    #[tokio::test]
    async fn on_error_fallback_route_continues_to_next_match() {
        assert_eq!(
            run(Mode::Err, OnError::FallbackRoute).await,
            Some(Routed::Pool("fallback".into()))
        );
        assert_eq!(
            run(Mode::Empty, OnError::FallbackRoute).await,
            Some(Routed::Pool("fallback".into()))
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

    /// End to end: a resolver `target` routes a real connection straight to an
    /// address that is in no pool.
    #[tokio::test]
    async fn resolver_target_routes_a_live_connection() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::{TcpListener, TcpStream};

        let one_off = {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = l.local_addr().unwrap();
            tokio::spawn(async move {
                if let Ok((mut s, _)) = l.accept().await {
                    let _ = s.write_all(b"T").await;
                    let mut b = [0u8; 8];
                    let _ = s.read(&mut b).await;
                }
            });
            addr
        };
        let fb = {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = l.local_addr().unwrap();
            tokio::spawn(async move {
                while let Ok((mut s, _)) = l.accept().await {
                    let _ = s.write_all(b"F").await;
                }
            });
            addr
        };
        let proxy = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap();

        let yaml = format!(
            r#"
pools:
  - {{ name: fallback, targets: ["{fb}"] }}
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
        let target_str: &'static str = Box::leak(one_off.to_string().into_boxed_str());
        resolvers.insert(
            "mm".to_string(),
            Arc::new(Stub {
                mode: Mode::Target(target_str),
                on_error: OnError::Reject,
            }) as Arc<dyn Resolver>,
        );
        let runtime =
            crate::Runtime::start(crate::Snapshot::from_config(&cfg), Arc::new(resolvers), 1);
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        let mut c = TcpStream::connect(proxy).await.unwrap();
        let mut m = [0u8; 1];
        c.read_exact(&mut m).await.unwrap();
        assert_eq!(m[0], b'T', "connected straight to the resolver target");

        runtime.shutdown().await;
    }

    // ---- cache ----

    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Counting {
        calls: AtomicUsize,
        mode: Mode,
        on_error: OnError,
    }
    #[async_trait]
    impl Resolver for Counting {
        fn name(&self) -> &str {
            "counting"
        }
        fn on_error(&self) -> OnError {
            self.on_error
        }
        async fn resolve(&self, _req: ResolveRequest) -> Result<Resolution, ResolveError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            mode_result(self.mode)
        }
    }

    fn cache_cfg(key: Vec<CacheKeyPart>) -> CacheConfig {
        CacheConfig {
            key,
            positive_ttl: Duration::from_secs(60),
            negative_ttl: Duration::from_secs(60),
            max_entries: 16,
        }
    }

    fn req(src: &str, sni: Option<&str>) -> ResolveRequest {
        ResolveRequest {
            listener: "l".into(),
            src: src.parse().unwrap(),
            dst: "1.1.1.1:7777".parse().unwrap(),
            sni: sni.map(str::to_string),
            first_bytes: vec![],
            routing_key: None,
        }
    }

    #[tokio::test]
    async fn cache_serves_repeats_and_keys_by_src_ip() {
        let inner = Arc::new(Counting {
            calls: AtomicUsize::new(0),
            mode: Mode::Pool("eu"),
            on_error: OnError::Reject,
        });
        let c = CachedResolver::new(inner.clone(), &cache_cfg(vec![CacheKeyPart::SrcIp]));

        assert_eq!(
            c.resolve(req("10.0.0.1:1", None))
                .await
                .unwrap()
                .pool
                .as_deref(),
            Some("eu")
        );
        assert_eq!(
            c.resolve(req("10.0.0.1:9", None))
                .await
                .unwrap()
                .pool
                .as_deref(),
            Some("eu")
        );
        assert_eq!(
            inner.calls.load(Ordering::SeqCst),
            1,
            "same src ip -> one upstream call"
        );

        // Different key -> another upstream call.
        let _ = c.resolve(req("10.0.0.2:1", None)).await;
        assert_eq!(inner.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn cache_negative_caches_failures() {
        let inner = Arc::new(Counting {
            calls: AtomicUsize::new(0),
            mode: Mode::Err,
            on_error: OnError::Reject,
        });
        let c = CachedResolver::new(inner.clone(), &cache_cfg(vec![CacheKeyPart::SrcIp]));
        assert!(c.resolve(req("10.0.0.1:1", None)).await.is_err());
        assert!(c.resolve(req("10.0.0.1:1", None)).await.is_err());
        assert_eq!(
            inner.calls.load(Ordering::SeqCst),
            1,
            "negative cache absorbs the retry"
        );
    }

    #[tokio::test]
    async fn cache_stale_ok_serves_expired_positive() {
        let inner = Arc::new(Counting {
            calls: AtomicUsize::new(0),
            mode: Mode::Pool("eu"),
            on_error: OnError::StaleOk,
        });
        let mut cfg = cache_cfg(vec![CacheKeyPart::SrcIp]);
        cfg.positive_ttl = Duration::from_millis(0); // immediately stale
        let mut c = CachedResolver::new(inner.clone(), &cfg);

        // Prime a (already-expired) positive entry.
        assert_eq!(
            c.resolve(req("10.0.0.1:1", None))
                .await
                .unwrap()
                .pool
                .as_deref(),
            Some("eu")
        );
        // Now make the upstream fail: stale_ok serves the expired positive.
        c.inner = Arc::new(Counting {
            calls: AtomicUsize::new(0),
            mode: Mode::Err,
            on_error: OnError::StaleOk,
        }) as Arc<dyn Resolver>;
        assert_eq!(
            c.resolve(req("10.0.0.1:1", None))
                .await
                .unwrap()
                .pool
                .as_deref(),
            Some("eu")
        );
    }

    #[tokio::test]
    async fn cache_passes_through_uncacheable_requests() {
        let inner = Arc::new(Counting {
            calls: AtomicUsize::new(0),
            mode: Mode::Pool("eu"),
            on_error: OnError::Reject,
        });
        // Key needs SNI, but the request has none -> never cached.
        let c = CachedResolver::new(inner.clone(), &cache_cfg(vec![CacheKeyPart::Sni]));
        let _ = c.resolve(req("10.0.0.1:1", None)).await;
        let _ = c.resolve(req("10.0.0.1:1", None)).await;
        assert_eq!(inner.calls.load(Ordering::SeqCst), 2);
    }
}
