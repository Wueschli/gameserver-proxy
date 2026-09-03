//! Runtime listener management: spawn / stop / rebind accept tasks as the
//! config changes, without restarting the process.
//!
//! One [`Group`] per configured listener holds its `workers` accept tasks (each
//! on its own `SO_REUSEPORT` socket) and a private stop channel.
//! [`ListenerManager::reconcile`] diffs the live [`Snapshot`]'s listeners
//! against the running groups by name:
//!
//! - name present, [`ListenerConfig`] unchanged → keep running;
//! - name present, config changed → stop the old group, spawn a new one (rebind);
//! - name only in the new config → spawn;
//! - name only in the running set → stop.
//!
//! Pool / route *contents* are still read live from the `ArcSwap<Snapshot>` by
//! the running tasks — only listener identity (bind, protocol, routes, …)
//! forces a respawn. The groups map is a plain `Mutex` that is never held across
//! an `.await` (stopped groups are moved out first, then awaited).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use gsp_config::{ListenerConfig, Protocol};

use crate::drain::ConnTracker;
use crate::resolver::Resolvers;
use crate::route_hint::RouteHints;
use crate::snapshot::Snapshot;

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
    workers: usize,
    groups: Mutex<HashMap<String, Group>>,
}

impl ListenerManager {
    pub fn new(
        snapshot: Arc<ArcSwap<Snapshot>>,
        hints: Arc<RouteHints>,
        conns: Arc<ConnTracker>,
        resolvers: Arc<Resolvers>,
        workers: usize,
    ) -> Arc<Self> {
        Arc::new(Self {
            snapshot,
            hints,
            conns,
            resolvers,
            workers,
            groups: Mutex::new(HashMap::new()),
        })
    }

    fn spawn_group(&self, cfg: &ListenerConfig) -> Group {
        let (stop_tx, stop_rx) = watch::channel(false);
        let mut tasks = Vec::with_capacity(self.workers);
        for worker_id in 0..self.workers {
            let snap = self.snapshot.clone();
            let hints = self.hints.clone();
            let conns = self.conns.clone();
            let resolvers = self.resolvers.clone();
            let lc = cfg.clone();
            let mut sd = stop_rx.clone();
            tasks.push(tokio::spawn(async move {
                let res = match lc.protocol {
                    Protocol::Tcp => {
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
                    Protocol::Udp => {
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
        Group {
            cfg: cfg.clone(),
            stop: stop_tx,
            tasks,
        }
    }

    /// Spawn groups for every listener in `snap`. Startup only — assumes no
    /// groups are running yet.
    pub fn start_all(&self, snap: &Snapshot) {
        let mut groups = self.groups.lock().unwrap();
        for lc in &snap.listeners {
            groups.insert(lc.name.clone(), self.spawn_group(lc));
        }
    }

    /// Bring the running listener set in line with `snap` (called after a
    /// reload). Returns the number of groups (re)started and stopped.
    pub async fn reconcile(&self, snap: &Snapshot) -> (usize, usize) {
        // Phase 1 (lock held, no await): remove stale groups, spawn new ones.
        let stopped: Vec<Group> = {
            let mut groups = self.groups.lock().unwrap();
            let wanted: HashMap<&str, &ListenerConfig> = snap
                .listeners
                .iter()
                .map(|l| (l.name.as_str(), l))
                .collect();

            let stale: Vec<String> = groups
                .iter()
                .filter(|(name, g)| match wanted.get(name.as_str()) {
                    Some(new_cfg) => **new_cfg != g.cfg,
                    None => true,
                })
                .map(|(name, _)| name.clone())
                .collect();
            let stopped: Vec<Group> = stale.iter().filter_map(|n| groups.remove(n)).collect();

            for lc in &snap.listeners {
                if !groups.contains_key(&lc.name) {
                    tracing::info!(
                        listener = %lc.name, bind = %lc.bind, protocol = ?lc.protocol,
                        "starting listener"
                    );
                    groups.insert(lc.name.clone(), self.spawn_group(lc));
                }
            }
            stopped
        };

        // Phase 2 (lock released): wait for the stopped groups' tasks.
        let n_stopped = stopped.len();
        for g in stopped {
            tracing::info!(listener = %g.cfg.name, bind = %g.cfg.bind, "stopping listener");
            g.stop().await;
        }
        let n_running = self.groups.lock().unwrap().len();
        (n_running, n_stopped)
    }

    /// Stop every listener group and wait for the accept tasks to finish.
    pub async fn stop_all(&self) {
        let drained: Vec<Group> = {
            let mut groups = self.groups.lock().unwrap();
            groups.drain().map(|(_, g)| g).collect()
        };
        for g in drained {
            g.stop().await;
        }
    }

    /// Best-effort abort of any still-running accept task (after a grace
    /// deadline expired).
    pub fn abort_all(&self) {
        for g in self.groups.lock().unwrap().values() {
            g.abort();
        }
    }
}
