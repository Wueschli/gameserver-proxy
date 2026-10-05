//! Active backend health checking (control plane).
//!
//! A single task sweeps every backend across every pool. Each backend carries
//! its own `interval` / `timeout` (from its pool's `health_check` config); the
//! sweep runs often and only probes backends that are due. Results feed
//! [`crate::pool::Backend::observe`], which applies the `rise` / `fall`
//! thresholds and flips the healthy flag.
//!
//! Probes run in the background, bounded by [`MAX_CONCURRENT_PROBES`], and the
//! sweep never waits for them, so one slow probe delays nobody else's check.
//! A probe that fails for a local reason (out of file descriptors, ports or
//! buffers) is "unknown" and counts neither for nor against the backend.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::{watch, Semaphore};
use tokio::task::JoinSet;
use tokio::time::{interval, MissedTickBehavior};
use wayhouse_config::HealthCheckKind;

use crate::gossip::GossipFabric;
use crate::metrics_defs as m;
use crate::snapshot::Snapshot;
use crate::util::{is_local_resource_error, mono_ms};

/// Sweep cadence. Actual per-backend probe frequency is governed by each
/// backend's own `check_interval`; this only bounds the resolution.
const SWEEP_PERIOD: Duration = Duration::from_millis(500);

/// Most active probes in flight at once; each holds a socket while it runs.
const MAX_CONCURRENT_PROBES: usize = 256;

/// Outcome of one active probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeResult {
    Ok,
    Fail,
    /// The probe could not run for a local reason; says nothing about the backend.
    Unknown,
}

impl ProbeResult {
    fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Fail => "fail",
            Self::Unknown => "unknown",
        }
    }
}

/// Clears a backend's in-flight mark when its probe task ends, however it ends.
struct ProbeSlot(Arc<crate::pool::Backend>);

impl Drop for ProbeSlot {
    fn drop(&mut self) {
        self.0.finish_probe();
    }
}

/// `gossip` is `Some` only when `settings.gossip` is set (phase 13, docs/10
/// "Tier 2"): every active-check result is also published into the mesh
/// (this instance's own opinion), and every backend's `domain_down` is
/// refreshed from the mesh's current quorum verdict — additive to the
/// existing local `rise`/`fall` logic, never replacing it.
pub async fn run(
    snapshot: Arc<ArcSwap<Snapshot>>,
    shutdown: &mut watch::Receiver<bool>,
    gossip: Option<GossipFabric>,
) {
    let permits = Arc::new(Semaphore::new(MAX_CONCURRENT_PROBES));
    let mut probes = JoinSet::new();
    let mut tick = interval(SWEEP_PERIOD);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    tracing::info!("health checker started");
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    tracing::info!("health checker stopping");
                    return;
                }
            }
            _ = tick.tick() => sweep(&snapshot, gossip.as_ref(), &permits, &mut probes),
        }
    }
}

fn sweep(
    snapshot: &Arc<ArcSwap<Snapshot>>,
    gossip: Option<&GossipFabric>,
    permits: &Arc<Semaphore>,
    probes: &mut JoinSet<()>,
) {
    // Reap finished probe tasks so the set does not grow without bound.
    while probes.try_join_next().is_some() {}

    let snap = snapshot.load_full();
    let now = mono_ms();

    for (pool_name, pool) in &snap.pools {
        for backend in pool.backends() {
            if *backend.check_kind() == HealthCheckKind::None {
                // No probe to run: an unhealthy backend gets a fresh chance each
                // interval, and only passive failures can take it down again.
                if backend.begin_probe(now) {
                    if !backend.locally_healthy() {
                        if let Some(new_state) = backend.observe(true) {
                            tracing::info!(
                                pool = %pool_name,
                                backend = %backend.addr,
                                healthy = new_state,
                                "backend health changed (retry, health_check none)"
                            );
                        }
                    }
                    backend.finish_probe();
                }
                continue;
            }
            if !backend.is_due(now) {
                continue;
            }
            // Out of permits: this one stays due and goes in a later sweep.
            let Ok(permit) = permits.clone().try_acquire_owned() else {
                continue;
            };
            if !backend.begin_probe(now) {
                continue;
            }
            let slot = ProbeSlot(backend.clone());
            let backend = backend.clone();
            let pool_name = pool_name.clone();
            let kind = backend.check_kind().clone();
            let gossip = gossip.cloned();
            probes.spawn(async move {
                let _permit = permit;
                let _slot = slot;
                let result = probe(backend.addr, backend.check_timeout(), &kind).await;
                metrics::counter!(
                    m::HEALTHCHECK,
                    "pool" => pool_name.clone(),
                    "backend" => backend.addr.to_string(),
                    "result" => result.label(),
                )
                .increment(1);
                if result == ProbeResult::Unknown {
                    return;
                }
                let ok = result == ProbeResult::Ok;
                if let Some(new_state) = backend.observe(ok) {
                    tracing::info!(
                        pool = %pool_name,
                        backend = %backend.addr,
                        healthy = new_state,
                        "backend health changed (active check)"
                    );
                }
                // This instance only ever asserts an opinion about a backend
                // it actually checks itself (docs/10 "Tier 2").
                if let Some(fabric) = &gossip {
                    fabric.handle.publish_backend_health(backend.addr, ok);
                }
            });
        }
    }

    if let Some(fabric) = gossip {
        for pool in snap.pools.values() {
            for backend in pool.backends() {
                let quorum_down = fabric
                    .handle
                    .quorum_down(backend.addr, fabric.quorum_fraction);
                if let Some(new_state) = backend.observe_domain(quorum_down) {
                    tracing::info!(
                        backend = %backend.addr,
                        healthy = new_state,
                        "backend health changed (domain quorum)"
                    );
                }
            }
        }
    }

    // Refresh the per-pool state gauges.
    for (pool_name, pool) in &snap.pools {
        let (mut healthy, mut unhealthy, mut draining, mut disabled) = (0i64, 0i64, 0i64, 0i64);
        for b in pool.backends() {
            match b.admin_state() {
                crate::pool::AdminState::Disabled => disabled += 1,
                crate::pool::AdminState::Draining => draining += 1,
                crate::pool::AdminState::Enabled if b.is_healthy() => healthy += 1,
                crate::pool::AdminState::Enabled => unhealthy += 1,
            }
        }
        metrics::gauge!(m::POOL_BACKENDS, "pool" => pool_name.clone(), "state" => "healthy")
            .set(healthy as f64);
        metrics::gauge!(m::POOL_BACKENDS, "pool" => pool_name.clone(), "state" => "unhealthy")
            .set(unhealthy as f64);
        metrics::gauge!(m::POOL_BACKENDS, "pool" => pool_name.clone(), "state" => "draining")
            .set(draining as f64);
        metrics::gauge!(m::POOL_BACKENDS, "pool" => pool_name.clone(), "state" => "disabled")
            .set(disabled as f64);
    }
}

/// A failed socket call: a local shortage says nothing about the backend.
fn failed(e: &std::io::Error) -> ProbeResult {
    if is_local_resource_error(e) {
        ProbeResult::Unknown
    } else {
        ProbeResult::Fail
    }
}

async fn probe(addr: SocketAddr, timeout: Duration, kind: &HealthCheckKind) -> ProbeResult {
    match kind {
        // `sweep` never probes a `none` backend.
        HealthCheckKind::None => ProbeResult::Unknown,
        HealthCheckKind::TcpConnect => {
            match tokio::time::timeout(timeout, TcpStream::connect(addr)).await {
                Ok(Ok(_)) => ProbeResult::Ok,
                Ok(Err(e)) => failed(&e),
                Err(_) => ProbeResult::Fail,
            }
        }
        HealthCheckKind::UdpProbe {
            send,
            expect_prefix,
        } => {
            let fut = async {
                let bind = if addr.is_ipv4() {
                    "0.0.0.0:0"
                } else {
                    "[::]:0"
                };
                // Binding is purely local: failure is never the backend's fault.
                let Ok(sock) = UdpSocket::bind(bind).await else {
                    return ProbeResult::Unknown;
                };
                if let Err(e) = sock.connect(addr).await {
                    return failed(&e);
                }
                if let Err(e) = sock.send(send).await {
                    return failed(&e);
                }
                let mut buf = [0u8; 2048];
                match sock.recv(&mut buf).await {
                    Ok(n)
                        if n > 0
                            && (expect_prefix.is_empty()
                                || buf[..n].starts_with(expect_prefix)) =>
                    {
                        ProbeResult::Ok
                    }
                    _ => ProbeResult::Fail,
                }
            };
            tokio::time::timeout(timeout, fut)
                .await
                .unwrap_or(ProbeResult::Fail)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One sweep, then wait for every probe it started.
    async fn sweep_and_settle(snap: &Arc<ArcSwap<Snapshot>>, gossip: Option<&GossipFabric>) {
        let permits = Arc::new(Semaphore::new(MAX_CONCURRENT_PROBES));
        let mut probes = JoinSet::new();
        sweep(snap, gossip, &permits, &mut probes);
        while probes.join_next().await.is_some() {}
    }

    fn snapshot_with_one_unreachable_backend() -> Arc<ArcSwap<Snapshot>> {
        // "127.0.0.1:1" is a privileged port nothing listens on — a
        // `TcpConnect` probe against it fails immediately (connection
        // refused), matching this crate's existing test convention
        // (`pool::tests::pcfg` uses the same address).
        let yaml = "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
                    listeners:\n  - name: l\n    bind: \"0.0.0.0:0\"\n    pool: p\n";
        let cfg = wayhouse_config::parse_str(yaml).unwrap();
        Arc::new(ArcSwap::from(Snapshot::from_config(&cfg)))
    }

    #[tokio::test]
    async fn without_gossip_sweep_never_touches_domain_down() {
        let snap = snapshot_with_one_unreachable_backend();
        sweep_and_settle(&snap, None).await;
        let backend = snap.load().pools["p"].backends()[0].clone();
        // One failed active check, `fall` defaults to 3 — still locally
        // healthy, and with no gossip fabric, domain_down can't have moved.
        assert!(backend.is_healthy());
    }

    #[cfg(feature = "gossip")]
    #[tokio::test]
    async fn sweep_publishes_and_then_reads_back_its_own_quorum_verdict() {
        let snap = snapshot_with_one_unreachable_backend();
        let backend = snap.load().pools["p"].backends()[0].clone();
        assert!(backend.is_healthy());

        // A real, single-node mesh (no seeds) — this is the full real
        // pipeline (channel -> gossip task -> add_broadcast ->
        // BroadcastMerger merge), not a pre-seeded map.
        let (handle, inbox) = crate::gossip::GossipHandle::new();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let cfg = wayhouse_config::GossipConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            seeds: vec![],
            quorum_fraction: 0.66,
            psk: "test-psk".to_string(),
        };
        let mesh = tokio::spawn(crate::gossip::run(cfg, handle.clone(), inbox, shutdown_rx));
        let fabric = GossipFabric {
            handle,
            quorum_fraction: 0.66,
        };

        // The first sweep probes (fails) and publishes "down"; every sweep
        // after that just re-reads the domain view (the backend isn't due
        // for another probe yet). The publish is processed asynchronously
        // by the mesh task, so it may take more than one sweep before this
        // instance's own vote has landed back in its own domain view —
        // poll rather than assume a fixed call count.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            sweep_and_settle(&snap, Some(&fabric)).await;
            if !backend.is_healthy() {
                break;
            }
            if tokio::time::Instant::now() > deadline {
                panic!("backend was never overridden down by the domain view");
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        // Still locally "healthy" (fall = 3, only ever probed once per
        // sweep) — the override is purely from the domain view.
        assert_eq!(fabric.handle.domain_votes(backend.addr), (0, 1));

        let _ = shutdown_tx.send(true);
        let _ = mesh.await;
    }

    fn snapshot_of_unreachable_backends(n: u16, extra: &str) -> Arc<ArcSwap<Snapshot>> {
        let targets: Vec<String> = (1..=n).map(|i| format!("\"127.0.0.1:{i}\"")).collect();
        let yaml = format!(
            "pools:\n  - name: p\n    targets: [{}]\n{extra}\
             listeners:\n  - name: l\n    bind: \"0.0.0.0:0\"\n    pool: p\n",
            targets.join(", ")
        );
        let cfg = wayhouse_config::parse_str(&yaml).unwrap();
        Arc::new(ArcSwap::from(Snapshot::from_config(&cfg)))
    }

    #[tokio::test]
    async fn a_sweep_starts_no_more_probes_than_it_has_permits() {
        let snap = snapshot_of_unreachable_backends(10, "");
        let snapshot = snap.load_full();
        for b in snapshot.pools["p"].backends() {
            b.force_due();
        }
        let permits = Arc::new(Semaphore::new(3));
        let mut probes = JoinSet::new();
        sweep(&snap, None, &permits, &mut probes);
        let now = mono_ms();
        let still_due = snapshot.pools["p"]
            .backends()
            .iter()
            .filter(|b| b.is_due(now))
            .count();
        assert_eq!(still_due, 7, "only the 3 probes with a permit were started");
        while probes.join_next().await.is_some() {}
        // Permits come back when the probes end, so the rest go next sweep.
        sweep(&snap, None, &permits, &mut probes);
        assert_eq!(
            snapshot.pools["p"]
                .backends()
                .iter()
                .filter(|b| b.is_due(now))
                .count(),
            4
        );
    }

    #[tokio::test]
    async fn a_slow_probe_does_not_hold_up_the_sweep() {
        // A black-hole address makes connect hang for the whole timeout; the
        // sweep must return immediately and not start a second probe for it.
        let yaml = "pools:\n  - name: p\n    targets: [\"10.255.255.1:9\"]\n    \
                    health_check: { interval_sec: 1, timeout_ms: 5000 }\n\
                    listeners:\n  - name: l\n    bind: \"0.0.0.0:0\"\n    pool: p\n";
        let cfg = wayhouse_config::parse_str(yaml).unwrap();
        let snap = Arc::new(ArcSwap::from(Snapshot::from_config(&cfg)));
        let backend = snap.load().pools["p"].backends()[0].clone();
        backend.force_due();
        let permits = Arc::new(Semaphore::new(MAX_CONCURRENT_PROBES));
        let mut probes = JoinSet::new();
        let started = std::time::Instant::now();
        sweep(&snap, None, &permits, &mut probes);
        assert!(started.elapsed() < Duration::from_millis(200));
        // Still in flight and past its interval: the next sweep skips it.
        backend.force_due();
        sweep(&snap, None, &permits, &mut probes);
        assert_eq!(probes.len(), 1);
    }

    #[tokio::test]
    async fn local_socket_failures_are_unknown_not_a_failed_check() {
        let emfile = std::io::Error::from_raw_os_error(24);
        assert_eq!(failed(&emfile), ProbeResult::Unknown);
        let refused = std::io::Error::from_raw_os_error(111);
        assert_eq!(failed(&refused), ProbeResult::Fail);
    }

    #[tokio::test]
    async fn health_check_none_gives_a_down_backend_another_chance() {
        let snap = snapshot_of_unreachable_backends(
            1,
            "    health_check: { type: none, interval_sec: 1, rise: 1, fall: 1 }\n",
        );
        let backend = snap.load().pools["p"].backends()[0].clone();
        backend.observe(false);
        assert!(!backend.is_healthy());
        backend.force_due();
        sweep_and_settle(&snap, None).await;
        assert!(backend.is_healthy(), "retried without any probe");
    }
}
