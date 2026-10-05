//! Runtime backend-discovery source management: spawn / stop / restart one
//! [`refresh_loop`](crate::discovery::refresh_loop) task per pool `source` as
//! the config changes, without restarting the process. The discovery analogue
//! of [`ListenerManager`](crate::listeners::ListenerManager).
//!
//! One [`SourceGroup`] per pool holds its refresh task and a private stop
//! channel. [`SourceManager::reconcile`] diffs the live [`Snapshot`]'s
//! `sources` map against the running groups by pool name:
//!
//! - pool present, [`SourceConfig`] unchanged → keep running;
//! - pool present, config changed → stop the old task, spawn a new one (an
//!   immediate first fetch overwrites the now-stale set);
//! - pool only in the new config → spawn;
//! - pool only in the running set → stop, and forget its last-known-good set.
//!
//! `wayhouse-core` stays HTTP-free: the concrete [`BackendSource`] adapters (DNS SRV,
//! Consul, Kubernetes) are built by a [`SourceFactory`] the `wayhouse` binary
//! supplies, exactly the seam resolvers and sniffers use.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::{watch, Notify};
use tokio::task::JoinHandle;

use wayhouse_config::SourceConfig;

use crate::discovery::{refresh_loop, BackendSource, Discovery};
use crate::error::SourceError;
use crate::snapshot::Snapshot;

/// Builds a concrete [`BackendSource`] from its config. Implemented in the `wayhouse`
/// binary (where the HTTP / DNS clients live); `wayhouse-core` only drives the
/// lifecycle.
pub trait SourceFactory: Send + Sync {
    fn build(&self, pool: &str, cfg: &SourceConfig) -> Result<Arc<dyn BackendSource>, SourceError>;
}

struct SourceGroup {
    cfg: SourceConfig,
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl SourceGroup {
    async fn stop(self) {
        let _ = self.stop.send(true);
        let _ = self.task.await;
    }

    fn abort(&self) {
        self.task.abort();
    }
}

pub struct SourceManager {
    discovery: Arc<Discovery>,
    reload: Arc<Notify>,
    factory: Arc<dyn SourceFactory>,
    groups: Mutex<HashMap<String, SourceGroup>>,
}

impl SourceManager {
    pub fn new(
        discovery: Arc<Discovery>,
        reload: Arc<Notify>,
        factory: Arc<dyn SourceFactory>,
    ) -> Arc<Self> {
        Arc::new(Self {
            discovery,
            reload,
            factory,
            groups: Mutex::new(HashMap::new()),
        })
    }

    fn spawn_group(&self, pool: &str, cfg: &SourceConfig) -> Result<SourceGroup, SourceError> {
        let source = self.factory.build(pool, cfg)?;
        let (stop_tx, mut stop_rx) = watch::channel(false);
        let discovery = self.discovery.clone();
        let reload = self.reload.clone();
        let task = tokio::spawn(async move {
            refresh_loop(source, discovery, reload, &mut stop_rx).await;
        });
        Ok(SourceGroup {
            cfg: cfg.clone(),
            stop: stop_tx,
            task,
        })
    }

    /// Spawn a refresh task for every pool `source` in `snap`. Startup only —
    /// assumes no groups are running yet. A source that fails to build is logged
    /// and skipped (the caller has already validated the specs and done a
    /// best-effort initial fetch); the pool then starts from its seed and the
    /// next reload can retry.
    pub fn start_all(&self, snap: &Snapshot) {
        let mut groups = self.groups.lock().unwrap_or_else(PoisonError::into_inner);
        for (pool, cfg) in &snap.sources {
            match self.spawn_group(pool, cfg) {
                Ok(g) => {
                    groups.insert(pool.clone(), g);
                }
                Err(e) => tracing::error!(
                    pool, error = %e,
                    "failed to build backend discovery source; pool starts from its seed"
                ),
            }
        }
    }

    /// Bring the running refresh tasks in line with `snap` (after a reload):
    /// spawn added sources, stop removed ones, restart changed ones. A source
    /// that fails to (re)build is logged and skipped — the pool keeps its
    /// last-known-good set. Returns `(running, stopped)`.
    pub async fn reconcile(&self, snap: &Snapshot) -> (usize, usize) {
        // Phase 1 (lock held, no await): remove stale groups, spawn new ones.
        let stopped: Vec<(String, SourceGroup)> = {
            let mut groups = self.groups.lock().unwrap_or_else(PoisonError::into_inner);

            let stale: Vec<String> = groups
                .iter()
                .filter(|(pool, g)| match snap.sources.get(pool.as_str()) {
                    Some(new_cfg) => *new_cfg != g.cfg,
                    None => true,
                })
                .map(|(pool, _)| pool.clone())
                .collect();
            let stopped: Vec<(String, SourceGroup)> = stale
                .iter()
                .filter_map(|p| groups.remove(p).map(|g| (p.clone(), g)))
                .collect();

            for (pool, cfg) in &snap.sources {
                if !groups.contains_key(pool) {
                    match self.spawn_group(pool, cfg) {
                        Ok(g) => {
                            tracing::info!(
                                pool, kind = ?cfg.kind,
                                "starting backend discovery source"
                            );
                            groups.insert(pool.clone(), g);
                        }
                        Err(e) => tracing::error!(
                            pool, error = %e,
                            "failed to (re)build backend discovery source; \
                             pool keeps its last-known-good backend set"
                        ),
                    }
                }
            }
            stopped
        };

        // Phase 2 (lock released): wait for the stopped tasks to unwind. A pool
        // that lost its `source` entirely also loses its cached set so a later
        // re-add starts clean.
        let n_stopped = stopped.len();
        for (pool, g) in stopped {
            tracing::info!(pool = %g.cfg.name, "stopping backend discovery source");
            g.stop().await;
            if !snap.sources.contains_key(&pool) {
                self.discovery.forget(&pool);
            }
        }
        let n_running = self
            .groups
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len();
        (n_running, n_stopped)
    }

    /// Stop every refresh task and wait for them to finish.
    pub async fn stop_all(&self) {
        let drained: Vec<SourceGroup> = {
            let mut groups = self.groups.lock().unwrap_or_else(PoisonError::into_inner);
            groups.drain().map(|(_, g)| g).collect()
        };
        for g in drained {
            g.stop().await;
        }
    }

    /// Best-effort abort of any still-running refresh task (after a grace
    /// deadline expired).
    pub fn abort_all(&self) {
        for g in self
            .groups
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
        {
            g.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use wayhouse_config::SourceKind;

    /// A fake source returning a fixed set; counts `fetch` calls so a test can
    /// tell a task actually started / stopped.
    struct FakeSource {
        pool: String,
        addrs: Vec<SocketAddr>,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl BackendSource for FakeSource {
        fn pool(&self) -> &str {
            &self.pool
        }
        fn kind(&self) -> &'static str {
            "dns_srv"
        }
        fn refresh_interval(&self) -> Duration {
            Duration::from_millis(20)
        }
        async fn fetch(&self) -> Result<Vec<SocketAddr>, SourceError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.addrs.clone())
        }
    }

    struct FakeFactory {
        calls: Arc<AtomicUsize>,
    }

    impl SourceFactory for FakeFactory {
        fn build(
            &self,
            pool: &str,
            cfg: &SourceConfig,
        ) -> Result<Arc<dyn BackendSource>, SourceError> {
            let addr: SocketAddr = match &cfg.kind {
                SourceKind::DnsSrv { record } => record.parse().unwrap(),
                _ => unreachable!("test only uses dns_srv"),
            };
            Ok(Arc::new(FakeSource {
                pool: pool.to_string(),
                addrs: vec![addr],
                calls: self.calls.clone(),
            }))
        }
    }

    fn src_cfg(name: &str, addr: &str) -> SourceConfig {
        SourceConfig {
            name: name.to_string(),
            kind: SourceKind::DnsSrv {
                record: addr.to_string(),
            },
            refresh_interval: Duration::from_millis(20),
        }
    }

    fn snap_with(sources: &[(&str, SourceConfig)]) -> Arc<Snapshot> {
        // Build a real snapshot then splice the sources map in — avoids wiring a
        // full YAML for every case.
        let cfg = wayhouse_config::parse_str(
            "pools:\n  - { name: p, targets: [\"127.0.0.1:1\"] }\n  - { name: q, targets: [\"127.0.0.1:2\"] }\n\
             listeners:\n  - { name: l, bind: \"0.0.0.0:7777\", pool: p }\n",
        )
        .unwrap();
        let mut s = Snapshot::from_config(&cfg);
        Arc::get_mut(&mut s).unwrap().sources = sources
            .iter()
            .map(|(p, c)| (p.to_string(), c.clone()))
            .collect();
        s
    }

    #[tokio::test]
    async fn reconcile_adds_restarts_and_removes_refresh_tasks() {
        let calls = Arc::new(AtomicUsize::new(0));
        let discovery = Arc::new(Discovery::new());
        let reload = Arc::new(Notify::new());
        let mgr = SourceManager::new(
            discovery.clone(),
            reload,
            Arc::new(FakeFactory {
                calls: calls.clone(),
            }),
        );

        // Start with one source for pool p.
        mgr.start_all(&snap_with(&[("p", src_cfg("eu", "10.0.0.1:7777"))]));
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(
            discovery.get("p").unwrap(),
            vec!["10.0.0.1:7777".parse().unwrap()]
        );

        // Reload: p re-parameterised (new address) + q added.
        let (running, stopped) = mgr
            .reconcile(&snap_with(&[
                ("p", src_cfg("eu", "10.0.0.2:7777")),
                ("q", src_cfg("us", "10.0.0.9:7777")),
            ]))
            .await;
        assert_eq!((running, stopped), (2, 1));
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(
            discovery.get("p").unwrap(),
            vec!["10.0.0.2:7777".parse().unwrap()]
        );
        assert_eq!(
            discovery.get("q").unwrap(),
            vec!["10.0.0.9:7777".parse().unwrap()]
        );

        // Reload: p removed, q unchanged (kept running, no restart).
        let calls_before = calls.load(Ordering::SeqCst);
        let (running, stopped) = mgr
            .reconcile(&snap_with(&[("q", src_cfg("us", "10.0.0.9:7777"))]))
            .await;
        assert_eq!((running, stopped), (1, 1));
        assert!(
            discovery.get("p").is_none(),
            "removed pool's set is forgotten"
        );
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(
            calls.load(Ordering::SeqCst) > calls_before,
            "the unchanged q task keeps polling"
        );

        mgr.stop_all().await;
        let after_stop = calls.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            after_stop,
            "no task polls after stop_all"
        );
    }
}
