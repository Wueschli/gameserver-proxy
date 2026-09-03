//! Owns the running listeners, the health checker, and the current config
//! snapshot.

use std::sync::Arc;

use arc_swap::ArcSwap;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::drain::{ConnTracker, DEFAULT_SHUTDOWN_GRACE};
use crate::resolver::Resolvers;
use crate::route_hint::RouteHints;
use crate::snapshot::Snapshot;

pub struct Runtime {
    snapshot: Arc<ArcSwap<Snapshot>>,
    hints: Arc<RouteHints>,
    conns: Arc<ConnTracker>,
    shutdown_tx: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
}

/// A cheap, cloneable handle to the live snapshot and the route-hint table.
/// Reads are lock-free; the reload task uses [`RuntimeHandle::store`] to swap in
/// a new snapshot.
#[derive(Clone)]
pub struct RuntimeHandle {
    snapshot: Arc<ArcSwap<Snapshot>>,
    hints: Arc<RouteHints>,
    conns: Arc<ConnTracker>,
}

impl RuntimeHandle {
    pub fn snapshot(&self) -> arc_swap::Guard<Arc<Snapshot>> {
        self.snapshot.load()
    }

    /// The current snapshot as an owned `Arc` (for building the next one).
    pub fn current(&self) -> Arc<Snapshot> {
        self.snapshot.load_full()
    }

    /// Atomically replace the live snapshot.
    pub fn store(&self, snapshot: Arc<Snapshot>) {
        self.snapshot.store(snapshot);
    }

    /// Ready once at least one listener is configured in the live snapshot.
    pub fn ready(&self) -> bool {
        !self.snapshot.load().listeners.is_empty()
    }

    /// The push-resolver hint table (`POST /route-hint`).
    pub fn route_hints(&self) -> &Arc<RouteHints> {
        &self.hints
    }

    /// Live proxied-connection count (TCP pumps + UDP sessions).
    pub fn active_conns(&self) -> usize {
        self.conns.active()
    }
}

impl Runtime {
    /// Spawn `workers` accept tasks per listener (one `SO_REUSEPORT` socket
    /// each) plus the health checker. `workers == 0` means one per CPU core.
    /// `resolvers` are the external routing resolvers, built from config by the
    /// caller (empty map = none).
    pub fn start(initial: Arc<Snapshot>, resolvers: Arc<Resolvers>, workers: usize) -> Self {
        let snapshot = Arc::new(ArcSwap::from(initial.clone()));
        let hints = RouteHints::new();
        let conns = ConnTracker::new();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let worker_count = if workers == 0 {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
        } else {
            workers
        };

        let mut tasks = Vec::new();
        for lc in &initial.listeners {
            for worker_id in 0..worker_count {
                let snap = snapshot.clone();
                let hints = hints.clone();
                let conns = conns.clone();
                let resolvers = resolvers.clone();
                let lc = lc.clone();
                let mut sd = shutdown_rx.clone();
                tasks.push(tokio::spawn(async move {
                    let res = match lc.protocol {
                        gsp_config::Protocol::Tcp => {
                            crate::listener::run_tcp_listener(
                                lc.clone(),
                                snap,
                                hints,
                                conns,
                                resolvers,
                                worker_id,
                                &mut sd,
                            )
                            .await
                        }
                        gsp_config::Protocol::Udp => {
                            crate::listener_udp::run_udp_listener(
                                lc.clone(),
                                snap,
                                hints,
                                conns,
                                resolvers,
                                worker_id,
                                &mut sd,
                            )
                            .await
                        }
                    };
                    if let Err(e) = res {
                        tracing::error!(
                            listener = %lc.name, worker = worker_id, error = %e,
                            "listener task exited with error"
                        );
                    }
                }));
            }
        }

        {
            let snap = snapshot.clone();
            let mut sd = shutdown_rx.clone();
            tasks.push(tokio::spawn(async move {
                crate::health::run(snap, &mut sd).await;
            }));
        }

        Self {
            snapshot,
            hints,
            conns,
            shutdown_tx,
            tasks,
        }
    }

    pub fn handle(&self) -> RuntimeHandle {
        RuntimeHandle {
            snapshot: self.snapshot.clone(),
            hints: self.hints.clone(),
            conns: self.conns.clone(),
        }
    }

    /// Stop accepting and drain in-flight connections, waiting up to
    /// [`DEFAULT_SHUTDOWN_GRACE`]. See [`Runtime::shutdown_with_grace`].
    pub async fn shutdown(self) {
        self.shutdown_with_grace(DEFAULT_SHUTDOWN_GRACE).await
    }

    /// Signal the accept loops and health checker to stop, wait for them, then
    /// wait for every tracked connection to finish — bounded by `grace`. When
    /// the grace period expires the still-running connections are left to be
    /// killed by the runtime shutdown that follows (the process is exiting).
    pub async fn shutdown_with_grace(mut self, grace: std::time::Duration) {
        let _ = self.shutdown_tx.send(true);

        let drained = {
            let conns = self.conns.clone();
            let tasks = &mut self.tasks;
            tokio::time::timeout(grace, async move {
                for t in tasks.iter_mut() {
                    let _ = t.await;
                }
                conns.wait_idle().await;
            })
            .await
            .is_ok()
        };

        if !drained {
            let n = self.conns.active();
            tracing::warn!(
                active = n,
                "shutdown grace expired; {n} connection(s) still in flight will be dropped"
            );
        }
        for t in &self.tasks {
            t.abort();
        }
    }
}
