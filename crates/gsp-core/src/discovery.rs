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
//! a `result` label on `gsp_discovery_refresh_total`).
//!
//! Concrete adapters (DNS SRV resolver, Consul HTTP, Kubernetes API) live in the
//! `gsp` binary and are injected at startup — `gsp-core` stays HTTP-free, the
//! same seam as resolvers and the backend overlay.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{watch, Notify};

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
    async fn fetch(&self) -> anyhow::Result<Vec<SocketAddr>>;

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
        let mut map = self.sets.lock().unwrap();
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
        self.sets.lock().unwrap().get(pool).cloned()
    }

    /// Drop the last-known-good set for `pool`. Called when a pool's `source` is
    /// removed on reload so a later re-add (or `GET`) starts from a clean slate
    /// rather than a stale set; harmless if the pool keeps a static target list.
    pub fn forget(&self, pool: &str) {
        self.sets.lock().unwrap().remove(pool);
    }
}

/// Poll one source on its interval, and again whenever it signals
/// [`BackendSource::changed`]; on a change, store it and wake the reload
/// task. Returns when `shutdown` flips to `true`.
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
            () = source.changed() => {}
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
}
