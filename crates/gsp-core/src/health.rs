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

use crate::metrics_defs as m;
use crate::snapshot::Snapshot;
use crate::util::now_ms;

/// Sweep cadence. Actual per-backend probe frequency is governed by each
/// backend's own `check_interval`; this only bounds the resolution.
const SWEEP_PERIOD: Duration = Duration::from_millis(500);

pub async fn run(snapshot: Arc<ArcSwap<Snapshot>>, shutdown: &mut watch::Receiver<bool>) {
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
            _ = tick.tick() => sweep(&snapshot).await,
        }
    }
}

async fn sweep(snapshot: &Arc<ArcSwap<Snapshot>>) {
    let snap = snapshot.load_full();
    let now = now_ms();

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
            }));
        }
    }
    for p in probes {
        let _ = p.await;
    }

    // Refresh the per-pool state gauges once probes have settled.
    for (pool_name, pool) in &snap.pools {
        let mut healthy = 0i64;
        let mut unhealthy = 0i64;
        for b in pool.backends() {
            if b.is_healthy() {
                healthy += 1;
            } else {
                unhealthy += 1;
            }
        }
        metrics::gauge!(m::POOL_BACKENDS, "pool" => pool_name.clone(), "state" => "healthy")
            .set(healthy as f64);
        metrics::gauge!(m::POOL_BACKENDS, "pool" => pool_name.clone(), "state" => "unhealthy")
            .set(unhealthy as f64);
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
