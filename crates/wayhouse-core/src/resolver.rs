//! External routing resolvers (phase 4).
//!
//! A route `action: { resolver: <name> }` hands the routing decision to an
//! out-of-process service. `wayhouse-core` owns the **contract** (this module) and
//! the routing integration ([`resolve_pool`]); the HTTP / gRPC clients that
//! actually speak to the service live in the `wayhouse` binary (keeps `wayhouse-core`
//! HTTP-free) and are injected at startup as `Arc<dyn Resolver>`.
//!
//! Done: HTTP + gRPC transports (in `wayhouse`), `pool` **and `target`** results,
//! `on_error` (`reject` / `fallback_route` / `stale_ok`), and the
//! [`CachedResolver`] TTL'd LRU cache. There is deliberately no resolver-chosen
//! affinity key: a later request cannot recover one without re-calling the
//! resolver, and `consistent_hash` pools plus the result cache already cover
//! affinity (#54).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use async_trait::async_trait;
use lru::LruCache;
use wayhouse_config::{
    Action, CacheConfig, CacheKeyPart, ListenerConfig, MatchContext, OnError, ProxyProtocol,
};

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
    /// PROXY protocol header to prepend when this resolver returns a `target`
    /// (a pool-less connect). Defaults to none; the transport clients override
    /// it from `ResolverConfig::proxy_protocol`.
    fn proxy_protocol(&self) -> ProxyProtocol {
        ProxyProtocol::None
    }
    /// Connect / idle timeout for a `target` result (a pool-less connect, so
    /// there is no pool to read `connect_timeout_ms` / `idle_timeout_sec`
    /// from). Default to `TARGET_CONNECT_TIMEOUT` / `TARGET_IDLE_TIMEOUT`; the
    /// transport clients override from `ResolverConfig`.
    fn target_connect_timeout(&self) -> Duration {
        crate::proxy::TARGET_CONNECT_TIMEOUT
    }
    fn target_idle_timeout(&self) -> Duration {
        crate::proxy::TARGET_IDLE_TIMEOUT
    }
    async fn resolve(&self, req: ResolveRequest) -> Result<Resolution, ResolveError>;
}

/// Name → resolver, built from config and shared across every listener worker.
///
/// Interior mutability via an [`ArcSwap`] over the map (mirroring
/// [`crate::sniff::Sniffers`]): every `Arc<Resolvers>` clone handed to a worker
/// at spawn time points at the *same* instance, so a config reload can swap the
/// whole set in with [`Resolvers::replace`] and every worker sees it at once —
/// no re-plumbing through `Runtime` / `ListenerManager`. A resolver call in
/// flight during a swap keeps its own `Arc<dyn Resolver>` (from
/// [`Resolvers::get`]) and finishes against the old client; new calls use the
/// new set. A rebuild drops each `CachedResolver`'s LRU cache, so there is a
/// brief cache-cold window after a `resolvers:` change (only then — the reload
/// task rebuilds solely when `ResolverConfig` actually differs).
#[derive(Default)]
pub struct Resolvers {
    map: ArcSwap<HashMap<String, Arc<dyn Resolver>>>,
}

impl Resolvers {
    pub fn new() -> Self {
        Self::default()
    }

    /// Wrap a ready name→resolver map (the startup build path).
    pub fn from_map(map: HashMap<String, Arc<dyn Resolver>>) -> Self {
        Self {
            map: ArcSwap::from_pointee(map),
        }
    }

    /// Insert one resolver (incremental build / tests).
    #[allow(clippy::needless_pass_by_value)] // registries take ownership of what they store
    pub fn insert(&self, name: String, resolver: Arc<dyn Resolver>) {
        self.map.rcu(|cur| {
            let mut next = (**cur).clone();
            next.insert(name.clone(), resolver.clone());
            next
        });
    }

    /// Atomically replace the whole set (config reload).
    pub fn replace(&self, map: HashMap<String, Arc<dyn Resolver>>) {
        self.map.store(Arc::new(map));
    }

    /// The resolver registered under `name`, as an owned handle the caller can
    /// hold across a concurrent [`Resolvers::replace`].
    pub fn get(&self, name: &str) -> Option<Arc<dyn Resolver>> {
        self.map.load().get(name).cloned()
    }

    pub fn is_empty(&self) -> bool {
        self.map.load().is_empty()
    }

    pub fn len(&self) -> usize {
        self.map.load().len()
    }
}

/// The outcome of routing: a pool (normal LB) or a fixed instance (`target`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Routed {
    Pool(String),
    /// A pool-less connect to a fixed instance. `proxy_protocol` is the header
    /// form the choosing resolver configured (`ProxyProtocol::None` for a
    /// push-hint or a direct-config target); `connect_timeout` / `idle_timeout`
    /// likewise come from the choosing resolver (or the `TARGET_*` defaults).
    Target {
        addr: SocketAddr,
        proxy_protocol: ProxyProtocol,
        connect_timeout: Duration,
        idle_timeout: Duration,
    },
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
                    sni: wayhouse_config::extract_sni(first),
                    first_bytes: first.to_vec(),
                    routing_key: mctx.sniff.and_then(|(_, h)| h.key.clone()),
                };
                match resolver.resolve(req).await {
                    Ok(res) => {
                        let routed = match (res.target, res.pool) {
                            (Some(addr), _) => Some(Routed::Target {
                                addr,
                                proxy_protocol: resolver.proxy_protocol(),
                                connect_timeout: resolver.target_connect_timeout(),
                                idle_timeout: resolver.target_idle_timeout(),
                            }),
                            (None, Some(pool)) => Some(Routed::Pool(pool)),
                            (None, None) => None,
                        };
                        if let Some(routed) = routed {
                            metrics::counter!(
                                m::RESOLVER_REQUESTS, "resolver" => name.clone(), "result" => "ok",
                            )
                            .increment(1);
                            return Some(routed);
                        }
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
    fn proxy_protocol(&self) -> ProxyProtocol {
        self.inner.proxy_protocol()
    }
    fn target_connect_timeout(&self) -> Duration {
        self.inner.target_connect_timeout()
    }
    fn target_idle_timeout(&self) -> Duration {
        self.inner.target_idle_timeout()
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
            let mut guard = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
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
                self.cache
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .put(
                        key,
                        Entry::Positive {
                            res: res.clone(),
                            expires: Instant::now() + ttl,
                        },
                    );
                Ok(res)
            }
            Ok(empty) => {
                self.cache
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .put(
                        key,
                        Entry::Negative {
                            expires: Instant::now() + self.negative_ttl,
                        },
                    );
                Ok(empty)
            }
            Err(e) => {
                if self.inner.on_error() == OnError::StaleOk {
                    let mut guard = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
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
                self.cache
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .put(
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
        let cfg = wayhouse_config::parse_str(
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
        let resolvers = Resolvers::new();
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

    /// `Resolvers::replace` swaps the live set — every worker's shared handle
    /// sees the new resolver on the next `resolve_route` (config reload path).
    #[tokio::test]
    async fn replace_swaps_the_live_resolver_set() {
        let lc = listener_with_resolver();
        let resolvers = Resolvers::new();
        resolvers.insert(
            "mm".to_string(),
            Arc::new(Stub {
                mode: Mode::Pool("old"),
                on_error: OnError::Reject,
            }) as Arc<dyn Resolver>,
        );
        let mctx = MatchContext {
            src: "9.9.9.9:1".parse().unwrap(),
            local: "1.1.1.1:7777".parse().unwrap(),
            first_bytes: &[],
            sniff: None,
        };
        assert_eq!(
            resolve_route(&lc, &resolvers, &mctx, &[]).await,
            Some(Routed::Pool("old".into()))
        );

        let mut next: HashMap<String, Arc<dyn Resolver>> = HashMap::new();
        next.insert(
            "mm".to_string(),
            Arc::new(Stub {
                mode: Mode::Pool("new"),
                on_error: OnError::Reject,
            }),
        );
        resolvers.replace(next);

        assert_eq!(
            resolve_route(&lc, &resolvers, &mctx, &[]).await,
            Some(Routed::Pool("new".into()))
        );
    }

    #[tokio::test]
    async fn resolver_target_result_is_used() {
        assert_eq!(
            run(Mode::Target("10.2.0.5:7777"), OnError::Reject).await,
            Some(Routed::Target {
                addr: "10.2.0.5:7777".parse().unwrap(),
                proxy_protocol: ProxyProtocol::None,
                connect_timeout: crate::proxy::TARGET_CONNECT_TIMEOUT,
                idle_timeout: crate::proxy::TARGET_IDLE_TIMEOUT,
            })
        );
    }

    /// A resolver's per-`target` connect / idle timeouts ride onto the
    /// `Routed::Target` (there is no pool to read them from).
    #[tokio::test]
    async fn resolver_target_carries_per_resolver_timeouts() {
        struct SlowTarget;
        #[async_trait]
        impl Resolver for SlowTarget {
            fn name(&self) -> &str {
                "slow"
            }
            fn on_error(&self) -> OnError {
                OnError::Reject
            }
            fn target_connect_timeout(&self) -> Duration {
                Duration::from_millis(1500)
            }
            fn target_idle_timeout(&self) -> Duration {
                Duration::from_secs(300)
            }
            async fn resolve(&self, _r: ResolveRequest) -> Result<Resolution, ResolveError> {
                Ok(Resolution {
                    target: Some("10.9.9.9:7777".parse().unwrap()),
                    ..Default::default()
                })
            }
        }

        let lc = listener_with_resolver();
        let resolvers = Resolvers::new();
        resolvers.insert("mm".to_string(), Arc::new(SlowTarget) as Arc<dyn Resolver>);
        let mctx = MatchContext {
            src: "9.9.9.9:1".parse().unwrap(),
            local: "1.1.1.1:7777".parse().unwrap(),
            first_bytes: &[],
            sniff: None,
        };
        assert_eq!(
            resolve_route(&lc, &resolvers, &mctx, &[]).await,
            Some(Routed::Target {
                addr: "10.9.9.9:7777".parse().unwrap(),
                proxy_protocol: ProxyProtocol::None,
                connect_timeout: Duration::from_millis(1500),
                idle_timeout: Duration::from_secs(300),
            })
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
        let cfg = wayhouse_config::parse_str(&yaml).unwrap();
        let resolvers = Resolvers::new();
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

        runtime
            .shutdown_with_grace(std::time::Duration::from_millis(100))
            .await;
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
        let cfg = wayhouse_config::parse_str(&yaml).unwrap();
        let resolvers = Resolvers::new();
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

        runtime
            .shutdown_with_grace(std::time::Duration::from_millis(100))
            .await;
    }

    /// A resolver with `proxy_protocol: v1` gets a PROXY protocol header written
    /// to its `target` connection before any client bytes — the pool-less path
    /// now carries the client IP too.
    #[tokio::test]
    async fn resolver_target_gets_a_proxy_protocol_header() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::{TcpListener, TcpStream};

        struct PpStub(std::net::SocketAddr);
        #[async_trait]
        impl Resolver for PpStub {
            fn name(&self) -> &str {
                "pp"
            }
            fn on_error(&self) -> OnError {
                OnError::Reject
            }
            fn proxy_protocol(&self) -> ProxyProtocol {
                ProxyProtocol::V1
            }
            async fn resolve(&self, _r: ResolveRequest) -> Result<Resolution, ResolveError> {
                Ok(Resolution {
                    target: Some(self.0),
                    ..Default::default()
                })
            }
        }

        let (tx, rx) = tokio::sync::oneshot::channel::<Vec<u8>>();
        let backend = {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = l.local_addr().unwrap();
            tokio::spawn(async move {
                let (mut s, _) = l.accept().await.unwrap();
                let mut buf = vec![0u8; 128];
                let n = s.read(&mut buf).await.unwrap();
                buf.truncate(n);
                let _ = tx.send(buf);
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
  - {{ name: fb, targets: ["127.0.0.1:1"] }}
resolvers:
  - {{ name: pp, endpoint: "http://unused" }}
listeners:
  - name: l
    bind: "{proxy}"
    routes:
      - {{ match: {{ type: always }}, action: {{ resolver: pp }} }}
      - {{ match: {{ type: always }}, action: {{ pool: fb }} }}
"#
        );
        let cfg = wayhouse_config::parse_str(&yaml).unwrap();
        let resolvers = Resolvers::new();
        resolvers.insert(
            "pp".to_string(),
            Arc::new(PpStub(backend)) as Arc<dyn Resolver>,
        );
        let runtime =
            crate::Runtime::start(crate::Snapshot::from_config(&cfg), Arc::new(resolvers), 1);

        // Retry the connect until the listener is up: a fixed sleep flakes
        // under parallel load. (Probing with a throwaway connection instead
        // would reach the stub backend and eat its single accept.)
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut c = loop {
            match TcpStream::connect(proxy).await {
                Ok(s) => break s,
                Err(e) if std::time::Instant::now() >= deadline => {
                    panic!("listener never came up: {e}")
                }
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
            }
        };
        let client_local = c.local_addr().unwrap();
        c.write_all(b"hello").await.unwrap();

        let got = tokio::time::timeout(std::time::Duration::from_secs(5), rx)
            .await
            .expect("backend never received bytes")
            .unwrap();
        let text = String::from_utf8_lossy(&got);
        assert!(text.starts_with("PROXY TCP4 "), "got {text:?}");
        assert!(
            text.contains(&format!(" {} ", client_local.port())),
            "header should carry the real client port; got {text:?}"
        );
        assert!(text.contains("\r\nhello"), "client bytes follow the header");

        runtime
            .shutdown_with_grace(std::time::Duration::from_millis(100))
            .await;
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
