//! Runtime listener management: spawn / stop / rebind accept tasks as the
//! config changes, without restarting the process.
//!
//! One [`Group`] per configured listener holds its `workers` accept tasks (each
//! on its own `SO_REUSEPORT` socket) and a private stop channel.
//! [`ListenerManager::reconcile`] diffs the live [`Snapshot`]'s listeners
//! against the running groups by name:
//!
//! - name present, [`ListenerConfig`] unchanged → keep running;
//! - name present, config changed → bind a new group, then stop the old one;
//! - name only in the new config → bind and spawn;
//! - name only in the running set → stop.
//!
//! Sockets are bound *before* anything is stopped or spawned, so a bind
//! failure is reported to the caller ([`BindError`]) instead of leaving a dead
//! listener: a changed listener that cannot rebind keeps its old group running
//! (and is retried on the next reconcile, since the running config still
//! differs), a new one is simply not started. Every bind address is first
//! probed for exclusivity ([`crate::net::probe_exclusive_tcp`]) because the
//! workers' `SO_REUSEPORT` would otherwise let a second process share the port
//! unnoticed; addresses this manager's own groups already hold are exempt (a
//! rebind of the same address, or two listeners swapping ports).
//!
//! Pool / route *contents* are still read live from the `ArcSwap<Snapshot>` by
//! the running tasks — only listener identity (bind, protocol, routes, …)
//! forces a respawn. The groups map is a plain `Mutex` that is never held across
//! an `.await` (stopped groups are moved out first, then awaited).

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};

use arc_swap::ArcSwap;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use wayhouse_config::{ListenerConfig, Protocol};

use crate::drain::ConnTracker;
use crate::geo::GeoDb;
use crate::limits::GlobalLimits;
use crate::ratelimit::RateLimiter;
use crate::resolver::Resolvers;
use crate::route_hint::RouteHints;
use crate::snapshot::Snapshot;
use crate::sniff::Sniffers;
use crate::src_conns::SourceLimiter;

/// A listener whose sockets could not be bound.
#[derive(Debug, thiserror::Error)]
#[error("listener {listener:?}: cannot bind {bind}: {source}")]
pub struct BindError {
    pub listener: String,
    pub bind: SocketAddr,
    #[source]
    pub source: std::io::Error,
}

/// Outcome of [`ListenerManager::reconcile`].
#[derive(Debug, Default)]
pub struct Reconciled {
    /// Listener groups running afterwards.
    pub running: usize,
    /// Groups stopped (removed listeners and successfully rebound ones).
    pub stopped: usize,
    /// Listeners that could not be bound; a failed rebind keeps its old group.
    pub failed: Vec<BindError>,
}

/// One bound socket per (bind address × worker), ready to hand to an accept task.
enum Socket {
    Tcp(std::net::TcpListener),
    Udp(std::net::UdpSocket),
}

struct Bound {
    cfg: ListenerConfig,
    /// `(address, worker id, socket)`.
    sockets: Vec<(SocketAddr, usize, Socket)>,
}

struct Group {
    cfg: ListenerConfig,
    stop: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
}

impl Group {
    /// Fire the stop channel and wait for the accept tasks to unwind.
    async fn stop(self) {
        let _ = self.stop.send(true);
        for t in self.tasks {
            let _ = t.await;
        }
    }

    fn abort(&self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

pub struct ListenerManager {
    snapshot: Arc<ArcSwap<Snapshot>>,
    hints: Arc<RouteHints>,
    conns: Arc<ConnTracker>,
    resolvers: Arc<Resolvers>,
    limits: Arc<GlobalLimits>,
    geo: Option<Arc<GeoDb>>,
    sniffers: Arc<Sniffers>,
    workers: usize,
    groups: Mutex<HashMap<String, Group>>,
}

impl ListenerManager {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        snapshot: Arc<ArcSwap<Snapshot>>,
        hints: Arc<RouteHints>,
        conns: Arc<ConnTracker>,
        resolvers: Arc<Resolvers>,
        limits: Arc<GlobalLimits>,
        geo: Option<Arc<GeoDb>>,
        sniffers: Arc<Sniffers>,
        workers: usize,
    ) -> Arc<Self> {
        Arc::new(Self {
            snapshot,
            hints,
            conns,
            resolvers,
            limits,
            geo,
            sniffers,
            workers,
            groups: Mutex::new(HashMap::new()),
        })
    }

    /// Probe every bind address for exclusivity (except those in `held`), then
    /// bind the worker sockets. Nothing is spawned or stopped.
    fn bind_group(
        &self,
        cfg: &ListenerConfig,
        held: &HashSet<SocketAddr>,
    ) -> Result<Bound, BindError> {
        let fail = |bind, source| BindError {
            listener: cfg.name.clone(),
            bind,
            source,
        };
        let mut sockets = Vec::new();
        for bind in cfg.binds() {
            // A port-range bind (F1.4) is one real socket set per port.
            let udp_mode = crate::listener_udp::udp_mode(cfg);
            if !held.contains(&bind) {
                match cfg.protocol {
                    Protocol::Tcp => {
                        crate::net::probe_exclusive_tcp(bind, cfg.freebind, cfg.transparent)
                    }
                    Protocol::Udp => crate::net::probe_exclusive_udp(bind, udp_mode),
                }
                .map_err(|e| fail(bind, e))?;
            }
            for worker_id in 0..self.workers {
                let sock = match cfg.protocol {
                    Protocol::Tcp => {
                        crate::net::bind_reuseport_tcp(bind, 1024, cfg.freebind, cfg.transparent)
                            .map(Socket::Tcp)
                    }
                    Protocol::Udp => {
                        crate::net::bind_reuseport_udp(bind, udp_mode).map(Socket::Udp)
                    }
                }
                .map_err(|e| fail(bind, e))?;
                sockets.push((bind, worker_id, sock));
            }
        }
        Ok(Bound {
            cfg: cfg.clone(),
            sockets,
        })
    }

    /// Addresses currently held by running groups.
    fn held(groups: &HashMap<String, Group>) -> HashSet<SocketAddr> {
        groups.values().flat_map(|g| g.cfg.binds()).collect()
    }

    fn spawn_group(&self, bound: Bound) -> Group {
        let Bound { cfg, sockets } = bound;
        let (stop_tx, stop_rx) = watch::channel(false);
        // One limiter per listener (shared across every port's workers, not
        // per-port — a port-range bind is still one logical listener), rebuilt
        // on every respawn so it tracks the live `ListenerConfig`.
        let limiter = Arc::new(RateLimiter::new(cfg.rate_limit.as_ref()));
        let src_limiter = SourceLimiter::new(cfg.per_source.as_ref());
        let mut tasks = Vec::with_capacity(sockets.len());
        for (bind, worker_id, socket) in sockets {
            // Every clone below carries `lc.bind` overridden to the one address
            // this task actually serves, so `run_tcp_listener` /
            // `run_udp_listener` need no change — route/filter/pool selection
            // still comes from the shared `lc`.
            let snap = self.snapshot.clone();
            let hints = self.hints.clone();
            let conns = self.conns.clone();
            let resolvers = self.resolvers.clone();
            let limiter = limiter.clone();
            let src_limiter = src_limiter.clone();
            let limits = self.limits.clone();
            let geo = self.geo.clone();
            let sniffers = self.sniffers.clone();
            let mut lc = cfg.clone();
            lc.bind = bind;
            let mut sd = stop_rx.clone();
            tasks.push(tokio::spawn(async move {
                let res = match socket {
                    Socket::Tcp(sock) => {
                        crate::listener::run_tcp_listener(
                            lc.clone(),
                            snap,
                            hints,
                            conns,
                            resolvers,
                            limiter,
                            src_limiter,
                            limits,
                            geo,
                            sniffers,
                            worker_id,
                            sock,
                            &mut sd,
                        )
                        .await
                    }
                    Socket::Udp(sock) => {
                        crate::listener_udp::run_udp_listener(
                            lc.clone(),
                            snap,
                            hints,
                            conns,
                            resolvers,
                            limiter,
                            src_limiter,
                            limits,
                            geo,
                            sniffers,
                            worker_id,
                            sock,
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
        Group {
            cfg,
            stop: stop_tx,
            tasks,
        }
    }

    /// Bind and spawn groups for every listener in `snap`. Startup only —
    /// assumes no groups are running yet. All-or-nothing: if any listener
    /// cannot bind, nothing is spawned and the first failure is returned.
    pub fn start_all(&self, snap: &Snapshot) -> Result<(), BindError> {
        let mut groups = self.groups.lock().unwrap_or_else(PoisonError::into_inner);
        let bound = snap
            .listeners
            .iter()
            .map(|lc| self.bind_group(lc, &HashSet::new()))
            .collect::<Result<Vec<_>, _>>()?;
        for b in bound {
            groups.insert(b.cfg.name.clone(), self.spawn_group(b));
        }
        Ok(())
    }

    /// Bring the running listener set in line with `snap` (called after a
    /// reload). A listener that cannot bind is reported in
    /// [`Reconciled::failed`] and leaves its previous group, if any, running.
    pub async fn reconcile(&self, snap: &Snapshot) -> Reconciled {
        // Phase 1 (lock held, no await): bind, then swap groups.
        let (stopped, failed): (Vec<Group>, Vec<BindError>) = {
            let mut groups = self.groups.lock().unwrap_or_else(PoisonError::into_inner);
            let wanted: HashMap<&str, &ListenerConfig> = snap
                .listeners
                .iter()
                .map(|l| (l.name.as_str(), l))
                .collect();

            let held = Self::held(&groups);
            let mut failed = Vec::new();
            let mut new_groups = Vec::new();
            for lc in &snap.listeners {
                if groups.get(&lc.name).is_some_and(|g| g.cfg == *lc) {
                    continue;
                }
                match self.bind_group(lc, &held) {
                    Ok(b) => new_groups.push(b),
                    Err(e) => {
                        tracing::error!(
                            listener = %lc.name, error = %e,
                            "cannot bind listener; keeping the previous one (if any) and retrying on the next reload"
                        );
                        failed.push(e);
                    }
                }
            }

            // Removed listeners, and changed ones that bound successfully.
            let stale: Vec<String> = groups
                .keys()
                .filter(|name| match wanted.get(name.as_str()) {
                    None => true,
                    Some(_) => new_groups.iter().any(|b| &b.cfg.name == *name),
                })
                .cloned()
                .collect();
            let stopped: Vec<Group> = stale.iter().filter_map(|n| groups.remove(n)).collect();
            for b in new_groups {
                tracing::info!(
                    listener = %b.cfg.name, bind = %b.cfg.bind, protocol = ?b.cfg.protocol,
                    "starting listener"
                );
                groups.insert(b.cfg.name.clone(), self.spawn_group(b));
            }
            (stopped, failed)
        };

        // Phase 2 (lock released): wait for the stopped groups' tasks.
        let n_stopped = stopped.len();
        for g in stopped {
            tracing::info!(listener = %g.cfg.name, bind = %g.cfg.bind, "stopping listener");
            g.stop().await;
        }
        let running = self
            .groups
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len();
        Reconciled {
            running,
            stopped: n_stopped,
            failed,
        }
    }

    /// Stop every listener group and wait for the accept tasks to finish.
    pub async fn stop_all(&self) {
        let drained: Vec<Group> = {
            let mut groups = self.groups.lock().unwrap_or_else(PoisonError::into_inner);
            groups.drain().map(|(_, g)| g).collect()
        };
        for g in drained {
            g.stop().await;
        }
    }

    /// Best-effort abort of any still-running accept task (after a grace
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
