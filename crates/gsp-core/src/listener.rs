//! The TCP listener accept loop. One instance of this task runs per worker per
//! listener, each with its own `SO_REUSEPORT` socket.

use std::sync::Arc;

use arc_swap::ArcSwap;
use tokio::net::TcpListener;
use tokio::sync::watch;

use gsp_config::ListenerConfig;

use crate::metrics_defs as m;
use crate::net::bind_reuseport_tcp;
use crate::resolver::{resolve_pool, Resolvers};
use crate::route_hint::RouteHints;
use crate::snapshot::Snapshot;

/// How long to wait for a client's first bytes when a route needs to peek them.
/// A client that connects but stays silent past this routes as if nothing was
/// sent (i.e. only address / `always` routes can match).
const PEEK_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

pub async fn run_tcp_listener(
    cfg: ListenerConfig,
    snapshot: Arc<ArcSwap<Snapshot>>,
    hints: Arc<RouteHints>,
    resolvers: Arc<Resolvers>,
    worker_id: usize,
    shutdown: &mut watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let cfg = Arc::new(cfg);
    let listener = TcpListener::from_std(bind_reuseport_tcp(cfg.bind, 1024, cfg.freebind)?)?;
    crate::sniff::warn_if_missing(&cfg.name, cfg.sniffer.as_deref());
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

                let listener_name = cfg.name.clone();
                metrics::counter!(
                    m::LISTENER_CONNECTIONS,
                    "listener" => listener_name.clone(),
                    "result" => "accepted",
                ).increment(1);

                let snap = snapshot.load_full();
                let cfg = cfg.clone();
                let hints = hints.clone();
                let resolvers = resolvers.clone();

                tokio::spawn(async move {
                    let local = stream.local_addr().unwrap_or(cfg.bind);

                    // Peek the first bytes only when a route needs them.
                    let peek_n = cfg.peek_len().min(gsp_config::PEEK_MAX);
                    let mut peek_buf = vec![0u8; peek_n];
                    let first: &[u8] = if peek_n > 0 {
                        match tokio::time::timeout(PEEK_TIMEOUT, stream.peek(&mut peek_buf)).await {
                            Ok(Ok(k)) => &peek_buf[..k],
                            _ => &[],
                        }
                    } else {
                        &[]
                    };

                    let hint = cfg
                        .sniffer
                        .as_deref()
                        .and_then(crate::sniff::sniffer)
                        .and_then(|s| s.sniff(first));
                    let mctx = gsp_config::MatchContext {
                        src: peer,
                        local,
                        first_bytes: first,
                        sniff: hint.as_ref(),
                    };
                    let hinted = cfg
                        .route_hint
                        .then(|| hints.lookup(peer.ip()))
                        .flatten()
                        .filter(|p| snap.pool(p).is_some());
                    if hinted.is_some() {
                        metrics::counter!(
                            m::ROUTE_HINTS_APPLIED, "listener" => listener_name.clone(),
                        ).increment(1);
                    }
                    let pool_name = match hinted {
                        Some(p) => Some(p),
                        None => resolve_pool(&cfg, &resolvers, &mctx, first).await,
                    };
                    let Some(pool_name) = pool_name else {
                        metrics::counter!(
                            m::LISTENER_CONNECTIONS,
                            "listener" => listener_name.clone(),
                            "result" => "no_route",
                        ).increment(1);
                        tracing::debug!(
                            listener = %listener_name, peer = %peer,
                            "no route matched; dropping connection"
                        );
                        return;
                    };
                    let Some(pool) = snap.pool(&pool_name) else {
                        metrics::counter!(
                            m::LISTENER_CONNECTIONS,
                            "listener" => listener_name.clone(),
                            "result" => "no_route",
                        ).increment(1);
                        tracing::error!(
                            listener = %listener_name, pool = %pool_name,
                            "routed pool missing from snapshot; dropping connection"
                        );
                        return;
                    };

                    metrics::gauge!(m::ACTIVE_CONNECTIONS, "listener" => listener_name.clone())
                        .increment(1.0);
                    let started = std::time::Instant::now();
                    match crate::proxy::handle_tcp(stream, peer, &pool).await {
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
