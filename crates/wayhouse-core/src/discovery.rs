//! Backend discovery: level-triggered address sources feeding the snapshot
//! (phase 8).
//!
//! A [`BackendSource`] returns *the current full set* of backend addresses for
//! one pool. The runtime never applies add/remove deltas — it diffs the
//! returned set against the live one (same model as
//! `ListenerManager::reconcile`), so a source that reuses a Tier-1 store later
//! plugs into the same path.
//!
//! One control-plane [`refresh_loop`] task runs per source, off the data path.
//! On a change it writes the new set into [`Discovery`] and asks the reload task
//! to rebuild the snapshot. A source that errors or returns an empty set does
//! **not** clear the pool: the last-known-good set stays in place (logged, plus
//! a `result` label on `wayhouse_discovery_refresh_total`).
//!
//! Concrete adapters (DNS SRV resolver, Consul HTTP, Kubernetes API) live in the
//! `wayhouse` binary and are injected at startup — `wayhouse-core` stays HTTP-free, the
//! same seam as resolvers and the backend overlay.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::{watch, Notify};

use crate::error::SourceError;
use crate::metrics_defs as m;

/// A level-triggered source of backend addresses for one pool.
#[async_trait::async_trait]
pub trait BackendSource: Send + Sync {
    /// The pool this source populates. Must match a `pools[].name`.
    fn pool(&self) -> &str;

    /// Short kind label for logs and the `kind` metric label
    /// (`dns_srv` | `consul` | `kubernetes`).
    fn kind(&self) -> &'static str;

    /// How often [`refresh_loop`] re-queries this source.
    fn refresh_interval(&self) -> Duration;

    /// The current full set of backend addresses. Level-triggered: the runtime
    /// diffs this against the live set. An `Err` (or an empty `Ok`) leaves the
    /// last-known-good set untouched.
    async fn fetch(&self) -> Result<Vec<SocketAddr>, SourceError>;

    /// Resolves when the source has reason to believe its set changed, so
    /// [`refresh_loop`] fetches right away instead of waiting out the
    /// interval. A push-capable source (a Kubernetes watch) overrides this;
    /// the default never resolves, which leaves pure interval polling.
    ///
    /// The interval tick stays as a resync safety net either way, so a
    /// missed or spurious signal only costs latency or one extra fetch.
    /// Must be cancel-safe: [`refresh_loop`] drops the future whenever the
    /// interval fires first, and calls it again after every fetch.
    async fn changed(&self) {
        std::future::pending::<()>().await;
    }
}

/// Last-known-good discovered address set per pool.
///
/// Layered *under* the [`BackendOverlay`](crate::overlay::BackendOverlay) during
/// a snapshot rebuild: for a pool with a `source`, the effective base target
/// list is its discovered set — falling back to the file `targets` seed until
/// the first successful fetch. Precedence, low to high:
/// `discovered set (or seed) → overlay added − overlay removed → health / admin state`.
#[derive(Debug, Default)]
pub struct Discovery {
    sets: Mutex<HashMap<String, Vec<SocketAddr>>>,
}

impl Discovery {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the known set for `pool` (sorted + deduped). Returns `true` if it
    /// differs from the set already stored.
    pub fn store(&self, pool: &str, mut addrs: Vec<SocketAddr>) -> bool {
        addrs.sort();
        addrs.dedup();
        let mut map = self.sets.lock().unwrap_or_else(PoisonError::into_inner);
        match map.get(pool) {
            Some(cur) if *cur == addrs => false,
            _ => {
                map.insert(pool.to_string(), addrs);
                true
            }
        }
    }

    /// The discovered set for `pool`, if a source has ever populated it.
    pub fn get(&self, pool: &str) -> Option<Vec<SocketAddr>> {
        self.sets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(pool)
            .cloned()
    }

    /// Drop the last-known-good set for `pool`. Called when a pool's `source` is
    /// removed on reload so a later re-add (or `GET`) starts from a clean slate
    /// rather than a stale set; harmless if the pool keeps a static target list.
    pub fn forget(&self, pool: &str) {
        self.sets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(pool);
    }
}

/// A pushed change is fetched once the source has been quiet this long.
pub const CHANGE_QUIET: Duration = Duration::from_millis(500);
/// Longest a steady stream of pushed changes can defer the fetch.
pub const CHANGE_MAX_WAIT: Duration = Duration::from_secs(5);

/// After a source's first [`BackendSource::changed`] signal, absorbs the
/// signals that follow — a rolling update is dozens of them — until the source
/// has been quiet for [`CHANGE_QUIET`] or [`CHANGE_MAX_WAIT`] has passed, so
/// the burst costs one fetch. `fetch` is level-triggered, so nothing the burst
/// carried is lost. Returns `false` if `shutdown` fired meanwhile.
async fn coalesce(source: &dyn BackendSource, shutdown: &mut watch::Receiver<bool>) -> bool {
    let deadline = tokio::time::Instant::now() + CHANGE_MAX_WAIT;
    loop {
        let quiet = (tokio::time::Instant::now() + CHANGE_QUIET).min(deadline);
        tokio::select! {
            () = source.changed() => {}
            () = tokio::time::sleep_until(quiet) => return true,
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    return false;
                }
            }
        }
    }
}

/// Poll one source on its interval, and again whenever it signals
/// [`BackendSource::changed`] (coalesced, see [`CHANGE_QUIET`]); on a change,
/// store it and wake the reload task. Returns when `shutdown` flips to `true`.
pub async fn refresh_loop(
    source: Arc<dyn BackendSource>,
    discovery: Arc<Discovery>,
    reload: Arc<Notify>,
    shutdown: &mut watch::Receiver<bool>,
) {
    let pool = source.pool().to_string();
    let kind = source.kind();
    let mut tick = tokio::time::interval(source.refresh_interval());
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = tick.tick() => {}
            () = source.changed() => {
                if !coalesce(&*source, shutdown).await {
                    return;
                }
            }
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    return;
                }
                continue;
            }
        }

        match source.fetch().await {
            Ok(addrs) if addrs.is_empty() => {
                tracing::warn!(
                    pool,
                    kind,
                    "discovery returned no addresses; keeping last-known-good backend set"
                );
                metrics::counter!(
                    m::DISCOVERY_REFRESH,
                    "pool" => pool.clone(), "kind" => kind, "result" => "empty"
                )
                .increment(1);
            }
            Ok(addrs) => {
                let n = addrs.len();
                let changed = discovery.store(&pool, addrs);
                metrics::counter!(
                    m::DISCOVERY_REFRESH,
                    "pool" => pool.clone(), "kind" => kind, "result" => "ok"
                )
                .increment(1);
                metrics::gauge!(m::DISCOVERY_BACKENDS, "pool" => pool.clone()).set(n as f64);
                if changed {
                    tracing::info!(
                        pool,
                        kind,
                        count = n,
                        "discovered backend set changed; requesting snapshot rebuild"
                    );
                    reload.notify_one();
                }
            }
            Err(e @ SourceError::Withdrawn { .. }) => {
                // The one error that is an answer: the set really is empty.
                // Clearing it stores `Some([])`, which wins over the file
                // `targets` seed.
                let changed = discovery.store(&pool, Vec::new());
                tracing::info!(pool, kind, error = %e, "source withdrew its backends; clearing the pool");
                metrics::counter!(
                    m::DISCOVERY_REFRESH,
                    "pool" => pool.clone(), "kind" => kind, "result" => "withdrawn"
                )
                .increment(1);
                metrics::gauge!(m::DISCOVERY_BACKENDS, "pool" => pool.clone()).set(0.0);
                if changed {
                    reload.notify_one();
                }
            }
            Err(e) => {
                tracing::warn!(
                    pool, kind, error = %e,
                    "discovery refresh failed; keeping last-known-good backend set"
                );
                metrics::counter!(
                    m::DISCOVERY_REFRESH,
                    "pool" => pool.clone(), "kind" => kind, "result" => "error"
                )
                .increment(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn store_reports_changes_and_normalises_order() {
        let d = Discovery::new();
        assert!(d.get("p").is_none());

        assert!(d.store("p", vec![a("127.0.0.1:2"), a("127.0.0.1:1")]));
        assert_eq!(
            d.get("p").unwrap(),
            vec![a("127.0.0.1:1"), a("127.0.0.1:2")]
        );

        // Same set, different order / dupes ⇒ no change.
        assert!(!d.store(
            "p",
            vec![a("127.0.0.1:2"), a("127.0.0.1:1"), a("127.0.0.1:2")]
        ));

        assert!(d.store("p", vec![a("127.0.0.1:1")]));
        assert_eq!(d.get("p").unwrap(), vec![a("127.0.0.1:1")]);
    }

    /// Push-only source: `changed()` resolves once per `poke`; `fetch` counts.
    struct Pushy {
        poked: Notify,
        fetches: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl BackendSource for Pushy {
        fn pool(&self) -> &str {
            "p"
        }
        fn kind(&self) -> &'static str {
            "test"
        }
        fn refresh_interval(&self) -> Duration {
            Duration::from_secs(3600)
        }
        async fn fetch(&self) -> Result<Vec<SocketAddr>, SourceError> {
            self.fetches
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(vec![a("127.0.0.1:1")])
        }
        async fn changed(&self) {
            self.poked.notified().await;
        }
    }

    fn fetches(src: &Pushy) -> usize {
        src.fetches.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Runs `refresh_loop` on paused time; returns the source and a stop switch.
    async fn spawn_pushy() -> (Arc<Pushy>, watch::Sender<bool>, tokio::task::JoinHandle<()>) {
        let src = Arc::new(Pushy {
            poked: Notify::new(),
            fetches: 0.into(),
        });
        let (tx, mut rx) = watch::channel(false);
        let task = tokio::spawn({
            let src = src.clone();
            async move {
                refresh_loop(
                    src,
                    Arc::new(Discovery::new()),
                    Arc::new(Notify::new()),
                    &mut rx,
                )
                .await;
            }
        });
        // The interval's first tick is immediate: the initial fetch.
        tokio::time::sleep(Duration::from_millis(1)).await;
        assert_eq!(fetches(&src), 1);
        (src, tx, task)
    }

    #[tokio::test(start_paused = true)]
    async fn a_burst_of_changes_costs_one_fetch_after_it_goes_quiet() {
        let (src, _tx, _task) = spawn_pushy().await;

        // Ten signals, 100 ms apart: each lands inside the previous quiet window.
        for _ in 0..10 {
            src.poked.notify_one();
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(fetches(&src), 1, "no fetch while the burst runs");

        tokio::time::sleep(CHANGE_QUIET).await;
        assert_eq!(fetches(&src), 2, "one fetch once it went quiet");

        // A later, separate change is fetched on its own.
        src.poked.notify_one();
        tokio::time::sleep(CHANGE_QUIET + Duration::from_millis(10)).await;
        assert_eq!(fetches(&src), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn a_steady_stream_cannot_defer_the_fetch_past_the_max_wait() {
        let (src, _tx, _task) = spawn_pushy().await;

        // A signal every 100 ms, for far longer than CHANGE_MAX_WAIT.
        for _ in 0..120 {
            src.poked.notify_one();
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            fetches(&src) >= 2,
            "the fetch must not starve under a continuous stream"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_ends_the_loop_while_a_burst_is_being_absorbed() {
        let (src, tx, task) = spawn_pushy().await;
        src.poked.notify_one();
        tokio::time::sleep(Duration::from_millis(50)).await;
        tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("loop stops")
            .unwrap();
        assert_eq!(fetches(&src), 1, "no fetch after shutdown");
    }
}
