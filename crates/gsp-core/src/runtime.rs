//! Owns the running listeners and the current config snapshot.

use std::sync::Arc;

use arc_swap::ArcSwap;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::snapshot::Snapshot;

pub struct Runtime {
    snapshot: Arc<ArcSwap<Snapshot>>,
    shutdown_tx: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
}

/// A cheap, cloneable handle for read-only access to the live snapshot
/// (used by the admin API).
#[derive(Clone)]
pub struct RuntimeHandle {
    snapshot: Arc<ArcSwap<Snapshot>>,
}

impl RuntimeHandle {
    pub fn snapshot(&self) -> arc_swap::Guard<Arc<Snapshot>> {
        self.snapshot.load()
    }

    /// Ready once at least one listener is configured in the live snapshot.
    pub fn ready(&self) -> bool {
        !self.snapshot.load().listeners.is_empty()
    }
}

impl Runtime {
    /// Spawn `workers` accept tasks per listener (one `SO_REUSEPORT` socket
    /// each). `workers == 0` means one per CPU core.
    pub fn start(initial: Arc<Snapshot>, workers: usize) -> Self {
        let snapshot = Arc::new(ArcSwap::from(initial.clone()));
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
                let lc = lc.clone();
                let mut sd = shutdown_rx.clone();
                tasks.push(tokio::spawn(async move {
                    if let Err(e) =
                        crate::listener::run_tcp_listener(lc.clone(), snap, worker_id, &mut sd)
                            .await
                    {
                        tracing::error!(
                            listener = %lc.name, worker = worker_id, error = %e,
                            "listener task exited with error"
                        );
                    }
                }));
            }
        }

        Self {
            snapshot,
            shutdown_tx,
            tasks,
        }
    }

    pub fn handle(&self) -> RuntimeHandle {
        RuntimeHandle {
            snapshot: self.snapshot.clone(),
        }
    }

    /// Signal every listener to stop accepting and wait for the accept tasks to
    /// finish. In-flight connection tasks are detached; a later slice adds a
    /// tracked drain with a grace period.
    pub async fn shutdown(self) {
        let _ = self.shutdown_tx.send(true);
        for t in self.tasks {
            let _ = t.await;
        }
    }
}
