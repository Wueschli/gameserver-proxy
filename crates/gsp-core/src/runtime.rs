//! Owns the running listeners, the health checker, and the current config
//! snapshot.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use arc_swap::ArcSwap;
use tokio::sync::{watch, Notify};
use tokio::task::JoinHandle;

use crate::discovery::Discovery;
use crate::drain::{ConnTracker, DEFAULT_SHUTDOWN_GRACE};
use crate::geo::GeoDb;
use crate::limits::GlobalLimits;
use crate::listeners::ListenerManager;
use crate::overlay::BackendOverlay;
use crate::resolver::Resolvers;
use crate::route_hint::RouteHints;
use crate::snapshot::Snapshot;
use crate::sniff::Sniffers;
use crate::sources::{SourceFactory, SourceManager};

pub struct Runtime {
    snapshot: Arc<ArcSwap<Snapshot>>,
    hints: Arc<RouteHints>,
    conns: Arc<ConnTracker>,
    /// Set by `POST /admin/drain` (and by shutdown): forces `readyz` to fail so
    /// an upstream LB / anycast takes this instance out of rotation while
    /// in-flight sessions keep running.
    draining: Arc<AtomicBool>,
    overlay: Arc<BackendOverlay>,
    /// Last-known-good discovered backend sets, consulted by every snapshot
    /// rebuild for pools with a `source`.
    discovery: Arc<Discovery>,
    listeners: Arc<ListenerManager>,
    /// One refresh task per pool `source`; reconciled on reload. `None` when no
    /// `SourceFactory` was supplied (i.e. tests / no discovery configured).
    sources: Option<Arc<SourceManager>>,
    reload_requested: Arc<Notify>,
    shutdown_tx: watch::Sender<bool>,
    /// The health-checker task. Listener tasks live in `listeners`, discovery
    /// refresh tasks in `sources`.
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
    draining: Arc<AtomicBool>,
    overlay: Arc<BackendOverlay>,
    discovery: Arc<Discovery>,
    listeners: Arc<ListenerManager>,
    sources: Option<Arc<SourceManager>>,
    reload_requested: Arc<Notify>,
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

    /// Ready once at least one listener is configured in the live snapshot and
    /// the instance has not been put into drain (`POST /admin/drain`).
    pub fn ready(&self) -> bool {
        !self.is_draining() && !self.snapshot.load().listeners.is_empty()
    }

    /// Whether `POST /admin/drain` (or a shutdown) has taken this instance out
    /// of rotation.
    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::Acquire)
    }

    /// Take the instance out of (`true`) or back into (`false`) LB rotation by
    /// flipping `readyz`. Does not stop the data path.
    pub fn set_draining(&self, on: bool) {
        self.draining.store(on, Ordering::Release);
    }

    /// The push-resolver hint table (`POST /route-hint`).
    pub fn route_hints(&self) -> &Arc<RouteHints> {
        &self.hints
    }

    /// The runtime backend overlay (`POST` / `DELETE /pools/{p}/backends`).
    /// After mutating it, call [`RuntimeHandle::request_reload`] so the snapshot
    /// is rebuilt.
    pub fn backend_overlay(&self) -> &Arc<BackendOverlay> {
        &self.overlay
    }

    /// The last-known-good discovered backend sets. The reload task passes this
    /// to [`Snapshot::build_with_sources`] so a rebuild picks up the newest
    /// discovery results.
    pub fn discovery(&self) -> &Arc<Discovery> {
        &self.discovery
    }

    /// Ask the reload task to rebuild the snapshot (after an overlay edit).
    pub fn request_reload(&self) {
        self.reload_requested.notify_one();
    }

    /// Awaited by the reload task; woken by [`RuntimeHandle::request_reload`].
    pub fn reload_requested(&self) -> &Arc<Notify> {
        &self.reload_requested
    }

    /// Bring the running listener set in line with the current snapshot:
    /// spawn added listeners, stop removed ones, rebind changed ones. Call
    /// after [`RuntimeHandle::store`]. Returns `(running, stopped)` counts.
    pub async fn reconcile_listeners(&self) -> (usize, usize) {
        let snap = self.snapshot.load_full();
        self.listeners.reconcile(&snap).await
    }

    /// Bring the running backend-discovery refresh tasks in line with the
    /// current snapshot: spawn added `pools[].source`s, stop removed ones,
    /// restart re-parameterised ones. Call after [`RuntimeHandle::store`].
    /// Returns `(running, stopped)`; `(0, 0)` when no `SourceManager` exists.
    pub async fn reconcile_sources(&self) -> (usize, usize) {
        match &self.sources {
            Some(mgr) => {
                let snap = self.snapshot.load_full();
                mgr.reconcile(&snap).await
            }
            None => (0, 0),
        }
    }

    /// Live proxied-connection count (TCP pumps + UDP sessions).
    pub fn active_conns(&self) -> usize {
        self.conns.active()
    }

    /// A point-in-time list of every live connection / session, for
    /// `GET /sessions`.
    pub fn sessions(&self) -> Vec<crate::drain::SessionInfo> {
        self.conns.sessions()
    }
}

impl Runtime {
    /// Spawn `workers` accept tasks per listener (one `SO_REUSEPORT` socket
    /// each) plus the health checker. `workers == 0` means one per CPU core.
    /// `resolvers` are the external routing resolvers, built from config by the
    /// caller (empty map = none).
    pub fn start(initial: Arc<Snapshot>, resolvers: Arc<Resolvers>, workers: usize) -> Self {
        Self::start_with_geo(initial, resolvers, None, workers)
    }

    /// Like [`Runtime::start`], with a preloaded GeoIP database for listeners
    /// that declare a `geo` filter. The binary opens it (and fails startup if
    /// the path is bad); tests pass `None`.
    pub fn start_with_geo(
        initial: Arc<Snapshot>,
        resolvers: Arc<Resolvers>,
        geo: Option<Arc<GeoDb>>,
        workers: usize,
    ) -> Self {
        Self::start_with_sniffers(
            initial,
            resolvers,
            geo,
            Arc::new(Sniffers::default()),
            workers,
        )
    }

    /// Like [`Runtime::start_with_geo`], with a preloaded sniffer plugin
    /// registry for listeners with a `sniffer:` route (phase 9). Empty by
    /// default — no sniffers ship in the binary; the `gsp` binary's plugin
    /// loader builds the registry.
    pub fn start_with_sniffers(
        initial: Arc<Snapshot>,
        resolvers: Arc<Resolvers>,
        geo: Option<Arc<GeoDb>>,
        sniffers: Arc<Sniffers>,
        workers: usize,
    ) -> Self {
        Self::start_with_discovery(
            initial,
            resolvers,
            geo,
            sniffers,
            Arc::new(Discovery::new()),
            None,
            workers,
        )
    }

    /// Like [`Runtime::start_with_sniffers`], plus backend discovery (phase 8):
    /// `discovery` holds the last-known-good address sets and, when a
    /// `source_factory` is supplied, a [`SourceManager`] runs one control-plane
    /// [`refresh_loop`](crate::discovery::refresh_loop) task per pool `source`
    /// and reconciles them on reload. The caller (the `gsp` binary) validates
    /// the specs and does a best-effort initial fetch into `discovery` before
    /// `initial` is built.
    #[allow(clippy::too_many_arguments)]
    pub fn start_with_discovery(
        initial: Arc<Snapshot>,
        resolvers: Arc<Resolvers>,
        geo: Option<Arc<GeoDb>>,
        sniffers: Arc<Sniffers>,
        discovery: Arc<Discovery>,
        source_factory: Option<Arc<dyn SourceFactory>>,
        workers: usize,
    ) -> Self {
        let snapshot = Arc::new(ArcSwap::from(initial.clone()));
        let hints = RouteHints::new();
        let conns = ConnTracker::new();
        let draining = Arc::new(AtomicBool::new(false));
        let overlay = Arc::new(BackendOverlay::new());
        let reload_requested = Arc::new(Notify::new());
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let worker_count = if workers == 0 {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
        } else {
            workers
        };

        let limits = GlobalLimits::new(&initial.limits);
        if geo.is_none() && initial.geo_db.is_some() {
            tracing::error!(
                "listeners with a `geo` filter will fail closed: geo DB not loaded \
                 (use Runtime::start_with_geo)"
            );
        }
        let listeners = ListenerManager::new(
            snapshot.clone(),
            hints.clone(),
            conns.clone(),
            resolvers,
            limits,
            geo,
            sniffers,
            worker_count,
        );
        listeners.start_all(&initial);

        let mut tasks = Vec::new();
        {
            let snap = snapshot.clone();
            let mut sd = shutdown_rx.clone();
            tasks.push(tokio::spawn(async move {
                crate::health::run(snap, &mut sd).await;
            }));
        }
        let sources = source_factory.map(|factory| {
            let mgr = SourceManager::new(discovery.clone(), reload_requested.clone(), factory);
            mgr.start_all(&initial);
            mgr
        });

        Self {
            snapshot,
            hints,
            conns,
            draining,
            overlay,
            discovery,
            listeners,
            sources,
            reload_requested,
            shutdown_tx,
            tasks,
        }
    }

    pub fn handle(&self) -> RuntimeHandle {
        RuntimeHandle {
            snapshot: self.snapshot.clone(),
            hints: self.hints.clone(),
            conns: self.conns.clone(),
            draining: self.draining.clone(),
            overlay: self.overlay.clone(),
            discovery: self.discovery.clone(),
            listeners: self.listeners.clone(),
            sources: self.sources.clone(),
            reload_requested: self.reload_requested.clone(),
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
        self.draining.store(true, Ordering::Release);
        let _ = self.shutdown_tx.send(true);

        let drained = {
            let conns = self.conns.clone();
            let listeners = self.listeners.clone();
            let sources = self.sources.clone();
            let tasks = &mut self.tasks;
            tokio::time::timeout(grace, async move {
                listeners.stop_all().await;
                if let Some(s) = &sources {
                    s.stop_all().await;
                }
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
        self.listeners.abort_all();
        if let Some(s) = &self.sources {
            s.abort_all();
        }
    }
}
