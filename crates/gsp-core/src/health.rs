//! Active backend health checking (control plane).
//!
//! A single task sweeps every backend across every pool. Each backend carries
//! its own `interval` / `timeout` (from its pool's `health_check` config); the
//! sweep runs often and only probes backends that are due. Results feed
//! [`crate::pool::Backend::observe`], which applies the `rise` / `fall`
//! thresholds and flips the healthy flag.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use gsp_config::HealthCheckKind;
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::watch;
use tokio::time::{interval, MissedTickBehavior};

use crate::gossip::GossipFabric;
use crate::metrics_defs as m;
use crate::snapshot::Snapshot;
use crate::util::mono_ms;

/// Sweep cadence. Actual per-backend probe frequency is governed by each
/// backend's own `check_interval`; this only bounds the resolution.
const SWEEP_PERIOD: Duration = Duration::from_millis(500);

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
            _ = tick.tick() => sweep(&snapshot, gossip.as_ref()).await,
        }
    }
}

async fn sweep(snapshot: &Arc<ArcSwap<Snapshot>>, gossip: Option<&GossipFabric>) {
    let snap = snapshot.load_full();
    let now = mono_ms();

    let mut probes = Vec::new();
    for (pool_name, pool) in &snap.pools {
        for backend in pool.backends() {
            if !backend.due_for_check(now) {
                continue;
            }
            backend.mark_checked(now);
            let backend = backend.clone();
            let pool_name = pool_name.clone();
            let kind = backend.check_kind().clone();
            let gossip = gossip.cloned();
            probes.push(tokio::spawn(async move {
                let ok = probe(backend.addr, backend.check_timeout(), &kind).await;
                metrics::counter!(
                    m::HEALTHCHECK,
                    "pool" => pool_name.clone(),
                    "backend" => backend.addr.to_string(),
                    "result" => if ok { "ok" } else { "fail" },
                )
                .increment(1);
                if let Some(new_state) = backend.observe(ok) {
                    tracing::info!(
                        pool = %pool_name,
                        backend = %backend.addr,
                        healthy = new_state,
                        "backend health changed (active check)"
                    );
                }
                // This instance only ever asserts an opinion about a backend
                // it actually checks itself (docs/10 "Tier 2") — exactly the
                // backends this sweep just probed.
                if let Some(fabric) = &gossip {
                    fabric.handle.publish_backend_health(backend.addr, ok);
                }
            }));
        }
    }
    for p in probes {
        let _ = p.await;
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

    // Refresh the per-pool state gauges once probes have settled.
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

async fn probe(addr: SocketAddr, timeout: Duration, kind: &HealthCheckKind) -> bool {
    match kind {
        HealthCheckKind::TcpConnect => matches!(
            tokio::time::timeout(timeout, TcpStream::connect(addr)).await,
            Ok(Ok(_))
        ),
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
                let sock = UdpSocket::bind(bind).await.ok()?;
                sock.connect(addr).await.ok()?;
                sock.send(send).await.ok()?;
                let mut buf = [0u8; 2048];
                let n = sock.recv(&mut buf).await.ok()?;
                Some(n > 0 && (expect_prefix.is_empty() || buf[..n].starts_with(expect_prefix)))
            };
            matches!(tokio::time::timeout(timeout, fut).await, Ok(Some(true)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot_with_one_unreachable_backend() -> Arc<ArcSwap<Snapshot>> {
        // "127.0.0.1:1" is a privileged port nothing listens on — a
        // `TcpConnect` probe against it fails immediately (connection
        // refused), matching this crate's existing test convention
        // (`pool::tests::pcfg` uses the same address).
        let yaml = "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
                    listeners:\n  - name: l\n    bind: \"0.0.0.0:0\"\n    pool: p\n";
        let cfg = gsp_config::parse_str(yaml).unwrap();
        Arc::new(ArcSwap::from(Snapshot::from_config(&cfg)))
    }

    #[tokio::test]
    async fn without_gossip_sweep_never_touches_domain_down() {
        let snap = snapshot_with_one_unreachable_backend();
        sweep(&snap, None).await;
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
        let cfg = gsp_config::GossipConfig {
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
            sweep(&snap, Some(&fabric)).await;
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
}
