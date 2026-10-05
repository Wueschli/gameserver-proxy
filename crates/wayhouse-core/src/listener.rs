//! The TCP listener accept loop. One instance of this task runs per worker per
//! listener, each with its own `SO_REUSEPORT` socket.

use std::sync::Arc;

use arc_swap::ArcSwap;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

use wayhouse_config::ListenerConfig;

use crate::drain::ConnTracker;
use crate::error::ListenerError;
use crate::geo::GeoDb;
use crate::limits::GlobalLimits;
use crate::metrics_defs as m;
use crate::ratelimit::RateLimiter;
use crate::resolver::{resolve_route, Resolvers, Routed};
use crate::route_hint::RouteHints;
use crate::snapshot::Snapshot;
use crate::sniff::Sniffers;
use crate::src_conns::SourceLimiter;

/// How long to wait for a client's first bytes when a route needs to peek them.
/// A client that connects but stays silent past this routes as if nothing was
/// sent (i.e. only address / `always` routes can match). Also the overall
/// budget for reassembling a TLS ClientHello split across TCP segments.
const PEEK_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

/// Gap between re-peeks while waiting for the rest of a fragmented ClientHello.
/// `TcpStream::peek` returns whatever is buffered *now* without waiting for more,
/// so a pause between calls is what actually yields to the next TCP segment
/// (rather than spinning until `PEEK_TIMEOUT`).
const PEEK_POLL: std::time::Duration = std::time::Duration::from_millis(5);

// Plumbing entry point: each argument is a distinct shared handle wired in by
// `ListenerManager::spawn_group` (its only caller), which also binds `socket`
// (see `net::bind_reuseport_tcp`) so a bind failure surfaces before any task runs. Bundling them would just
// move the list into a struct literal there.
#[allow(clippy::too_many_arguments)]
pub async fn run_tcp_listener(
    cfg: ListenerConfig,
    snapshot: Arc<ArcSwap<Snapshot>>,
    hints: Arc<RouteHints>,
    conns: Arc<ConnTracker>,
    resolvers: Arc<Resolvers>,
    limiter: Arc<RateLimiter>,
    src_limiter: Arc<SourceLimiter>,
    limits: Arc<GlobalLimits>,
    geo: Option<Arc<GeoDb>>,
    sniffers: Arc<Sniffers>,
    worker_id: usize,
    socket: std::net::TcpListener,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<(), ListenerError> {
    let cfg = Arc::new(cfg);
    let listener = TcpListener::from_std(socket)?;
    crate::sniff::warn_if_missing(&cfg.name, &cfg.sniffers, &sniffers);
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

                // Filter chain (phase 7): drop connections from a denied source
                // IP before any bookkeeping or task spawn.
                if !cfg.acl.permits(peer.ip()) {
                    metrics::counter!(
                        m::FILTER_BLOCKED,
                        "listener" => listener_name.clone(), "filter" => "acl",
                    ).increment(1);
                    tracing::debug!(listener = %listener_name, peer = %peer, "connection blocked by acl");
                    continue;
                }
                if let Some(geo_acl) = &cfg.geo {
                    let admitted = geo
                        .as_deref()
                        .map(|db| geo_acl.permits(db.country_code(peer.ip())))
                        .unwrap_or(false); // fail closed if the DB did not load
                    if !admitted {
                        metrics::counter!(
                            m::FILTER_BLOCKED,
                            "listener" => listener_name.clone(), "filter" => "geo",
                        ).increment(1);
                        tracing::debug!(listener = %listener_name, peer = %peer, "connection blocked by geo filter");
                        continue;
                    }
                }
                if let Some(which) = limiter.permit(peer.ip()) {
                    metrics::counter!(
                        m::FILTER_BLOCKED,
                        "listener" => listener_name.clone(), "filter" => which,
                    ).increment(1);
                    tracing::debug!(
                        listener = %listener_name, peer = %peer, bucket = which,
                        "connection dropped by rate limit"
                    );
                    continue;
                }
                // Per-source concurrent cap (per listener).
                let src_guard = match src_limiter.acquire(peer.ip()) {
                    Ok(g) => g,
                    Err(which) => {
                        metrics::counter!(
                            m::FILTER_BLOCKED,
                            "listener" => listener_name.clone(), "filter" => which,
                        ).increment(1);
                        tracing::debug!(
                            listener = %listener_name, peer = %peer, cap = which,
                            "connection dropped by a per-source cap"
                        );
                        continue;
                    }
                };
                // Global caps (process-wide): refuse before allocating anything.
                let limit_guard = match limits.acquire_tcp() {
                    Ok(g) => g,
                    Err(which) => {
                        metrics::counter!(
                            m::FILTER_BLOCKED,
                            "listener" => listener_name.clone(), "filter" => which,
                        ).increment(1);
                        tracing::debug!(
                            listener = %listener_name, peer = %peer, cap = which,
                            "connection dropped by a global cap"
                        );
                        continue;
                    }
                };

                metrics::counter!(
                    m::LISTENER_CONNECTIONS,
                    "listener" => listener_name.clone(),
                    "result" => "accepted",
                ).increment(1);

                let snap = snapshot.load_full();
                let cfg = cfg.clone();
                let hints = hints.clone();
                let resolvers = resolvers.clone();
                let sniffers = sniffers.clone();
                let local = stream.local_addr().unwrap_or(cfg.bind);
                let conn_guard = conns.track(crate::drain::SessionMeta {
                    proto: crate::drain::Proto::Tcp,
                    listener: listener_name.clone(),
                    peer,
                    local,
                });

                tokio::spawn(async move {
                    // Held for the whole connection so a graceful shutdown waits
                    // for it; dropped when this task returns.
                    let _conn_guard = conn_guard;
                    // Releases the global `max_connections` slot on task exit.
                    let _limit_guard = limit_guard;
                    // Releases the per-source concurrent slot on task exit.
                    let _src_guard = src_guard;

                    // Peek the first bytes only when a route needs them,
                    // reassembling a TLS ClientHello that spans TCP segments.
                    let peek_n = cfg.peek_len().min(wayhouse_config::PEEK_MAX);
                    let mut peek_buf = vec![0u8; peek_n];
                    let first = peek_routing_bytes(&stream, &mut peek_buf, peek_n).await;

                    let hit = crate::sniff::sniff_first(&cfg.sniffers, &sniffers, first);

                    // A sniffer that positively rejects drops the connection
                    // outright — checked before the push-resolver hint so a
                    // spoofable src_ip hint can't override a content-based
                    // reject (same rule as the UDP first-packet gate).
                    if hit.as_ref().is_some_and(|(_, h)| h.reject) {
                        metrics::counter!(
                            m::LISTENER_CONNECTIONS,
                            "listener" => listener_name.clone(),
                            "result" => "sniffer_reject",
                        ).increment(1);
                        tracing::debug!(
                            listener = %listener_name, peer = %peer,
                            "connection dropped by sniffer reject"
                        );
                        return;
                    }

                    let mctx = wayhouse_config::MatchContext {
                        src: peer,
                        local,
                        first_bytes: first,
                        sniff: hit.as_ref().map(|(n, h)| (*n, h)),
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
                    let routed = match hinted {
                        Some(p) => Some(Routed::Pool(p)),
                        None => resolve_route(&cfg, &resolvers, &mctx, first).await,
                    };
                    let Some(routed) = routed else {
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

                    // For a pool route, resolve it against the live snapshot now.
                    let pool = match &routed {
                        Routed::Target { .. } => None,
                        Routed::Pool(name) => match snap.pool(name) {
                            Some(p) => Some(p),
                            None => {
                                metrics::counter!(
                                    m::LISTENER_CONNECTIONS,
                                    "listener" => listener_name.clone(),
                                    "result" => "no_route",
                                ).increment(1);
                                tracing::error!(
                                    listener = %listener_name, pool = %name,
                                    "routed pool missing from snapshot; dropping connection"
                                );
                                return;
                            }
                        },
                    };

                    metrics::gauge!(m::ACTIVE_CONNECTIONS, "listener" => listener_name.clone())
                        .increment(1.0);
                    let started = std::time::Instant::now();
                    // Transparent mode: bind the real client address as the
                    // upstream source so the backend sees the client IP.
                    let tsrc = cfg.transparent.then_some(peer);
                    let result = match (&routed, &pool) {
                        (
                            Routed::Target {
                                addr,
                                proxy_protocol,
                                connect_timeout,
                                idle_timeout,
                            },
                            _,
                        ) => crate::proxy::handle_tcp_target(
                            stream,
                            peer,
                            local,
                            *addr,
                            *connect_timeout,
                            *idle_timeout,
                            tsrc,
                            &_conn_guard,
                            *proxy_protocol,
                        )
                        .await,
                        (_, Some(pool)) => {
                            crate::proxy::handle_tcp(stream, peer, local, tsrc, &_conn_guard, pool)
                                .await
                        }
                        _ => unreachable!("pool route always resolves a pool above"),
                    };
                    match result {
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

/// Peek up to `want` of the connection's first bytes for routing, without
/// consuming them from the stream.
///
/// A single `TcpStream::peek` only returns what is buffered at that instant,
/// which can be just the first TCP segment. A real TLS ClientHello frequently
/// spans more than one segment (large ALPN / key-share / ECH extensions, or a
/// client that writes it in pieces), and `extract_sni` needs the whole first
/// TLS record to read the SNI. So when the first byte is a TLS handshake record
/// (`0x16`) this re-peeks — every `PEEK_POLL`, bounded by `PEEK_TIMEOUT` — until
/// the complete first record is buffered or `want` bytes are in hand.
///
/// Non-TLS first bytes keep the original single-peek behaviour: a `first_bytes`
/// `prefix` fits inside the first segment, and the `length` window is documented
/// (`docs/03`, `Matcher::FirstBytes`) as "what one peek returned".
///
/// Returns the bytes peeked (borrowing `buf`); empty if the client sent nothing
/// within the budget.
async fn peek_routing_bytes<'b>(stream: &TcpStream, buf: &'b mut [u8], want: usize) -> &'b [u8] {
    if want == 0 {
        return &[];
    }
    let deadline = tokio::time::Instant::now() + PEEK_TIMEOUT;
    let mut have = 0usize;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, stream.peek(&mut buf[..want])).await {
            // EOF, a read error, or the budget ran out: route on what we have.
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
            Ok(Ok(n)) => {
                have = n;
                if n >= want || routing_bytes_complete(&buf[..n]) {
                    break;
                }
            }
        }
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            break;
        }
        tokio::time::sleep(PEEK_POLL.min(left)).await;
    }
    &buf[..have]
}

/// Whether `buf` already holds everything routing could learn by waiting for
/// more bytes. True unless `buf` is the start of a TLS handshake record whose
/// declared length is not yet fully buffered.
fn routing_bytes_complete(buf: &[u8]) -> bool {
    match buf.first() {
        // TLS handshake record: byte 0 = 0x16, bytes 3..5 = record length.
        Some(&0x16) => {
            let Some(len_bytes) = buf.get(3..5) else {
                return false; // not even the 5-byte record header yet
            };
            let rec_len = u16::from_be_bytes([len_bytes[0], len_bytes[1]]) as usize;
            buf.len() >= 5 + rec_len
        }
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::routing_bytes_complete;

    #[test]
    fn non_tls_first_byte_is_always_complete() {
        assert!(routing_bytes_complete(b"GET / HTTP/1.1"));
        assert!(routing_bytes_complete(&[0xff, 0xff, 0xff, 0xff]));
        assert!(routing_bytes_complete(b"")); // nothing peeked yet, nothing to wait on
    }

    #[test]
    fn tls_record_needs_the_full_first_record() {
        // 0x16, version, u16 length = 10, then fewer than 10 body bytes.
        assert!(!routing_bytes_complete(&[0x16, 0x03, 0x01, 0x00]));
        assert!(!routing_bytes_complete(&[
            0x16, 0x03, 0x01, 0x00, 0x0a, 1, 2, 3
        ]));
        // Header says 3 body bytes and all 3 are present.
        assert!(routing_bytes_complete(&[
            0x16, 0x03, 0x01, 0x00, 0x03, 1, 2, 3
        ]));
        // ... and trailing bytes past the first record are still "complete".
        assert!(routing_bytes_complete(&[
            0x16, 0x03, 0x01, 0x00, 0x03, 1, 2, 3, 4, 5
        ]));
    }
}
