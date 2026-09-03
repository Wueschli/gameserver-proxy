//! The TCP listener accept loop. One instance of this task runs per worker per
//! listener, each with its own `SO_REUSEPORT` socket.

use std::sync::Arc;

use arc_swap::ArcSwap;
use tokio::net::TcpListener;
use tokio::sync::watch;

use gsp_config::ListenerConfig;

use crate::metrics_defs as m;
use crate::net::bind_reuseport_tcp;
use crate::snapshot::Snapshot;

pub async fn run_tcp_listener(
    cfg: ListenerConfig,
    snapshot: Arc<ArcSwap<Snapshot>>,
    worker_id: usize,
    shutdown: &mut watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let listener = TcpListener::from_std(bind_reuseport_tcp(cfg.bind, 1024)?)?;
    tracing::info!(
        listener = %cfg.name,
        worker = worker_id,
        bind = %cfg.bind,
        "tcp listener started"
    );

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    tracing::info!(listener = %cfg.name, worker = worker_id, "listener stopping");
                    return Ok(());
                }
            }
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(listener = %cfg.name, error = %e, "accept failed");
                        continue;
                    }
                };

                let snap = snapshot.load_full();
                let local = stream.local_addr().unwrap_or(cfg.bind);
                let Some(pool_name) = cfg.route_for(peer, local) else {
                    metrics::counter!(
                        m::LISTENER_CONNECTIONS,
                        "listener" => cfg.name.clone(),
                        "result" => "no_route",
                    ).increment(1);
                    tracing::debug!(
                        listener = %cfg.name, peer = %peer,
                        "no route matched; dropping connection"
                    );
                    continue;
                };
                let Some(pool) = snap.pool(pool_name) else {
                    metrics::counter!(
                        m::LISTENER_CONNECTIONS,
                        "listener" => cfg.name.clone(),
                        "result" => "no_route",
                    ).increment(1);
                    tracing::error!(
                        listener = %cfg.name, pool = %pool_name,
                        "routed pool missing from snapshot; dropping connection"
                    );
                    continue;
                };

                let listener_name = cfg.name.clone();
                metrics::counter!(
                    m::LISTENER_CONNECTIONS,
                    "listener" => listener_name.clone(),
                    "result" => "accepted",
                ).increment(1);
                metrics::gauge!(m::ACTIVE_CONNECTIONS, "listener" => listener_name.clone())
                    .increment(1.0);

                tokio::spawn(async move {
                    let started = std::time::Instant::now();
                    match crate::proxy::handle_tcp(stream, &pool).await {
                        Ok(out) => {
                            metrics::counter!(
                                m::BYTES, "listener" => listener_name.clone(), "dir" => "c2s",
                            ).increment(out.bytes_c2s);
                            metrics::counter!(
                                m::BYTES, "listener" => listener_name.clone(), "dir" => "s2c",
                            ).increment(out.bytes_s2c);
                            tracing::info!(
                                listener = %listener_name,
                                peer = %peer,
                                backend = %out.backend,
                                bytes_c2s = out.bytes_c2s,
                                bytes_s2c = out.bytes_s2c,
                                duration_ms = started.elapsed().as_millis() as u64,
                                "connection closed"
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                listener = %listener_name, peer = %peer, error = %e,
                                "connection failed"
                            );
                        }
                    }
                    metrics::histogram!(m::CONNECTION_DURATION, "listener" => listener_name.clone())
                        .record(started.elapsed().as_secs_f64());
                    metrics::gauge!(m::ACTIVE_CONNECTIONS, "listener" => listener_name)
                        .decrement(1.0);
                });
            }
        }
    }
}
