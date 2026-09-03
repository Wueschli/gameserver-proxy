//! The UDP listener and its per-worker session table.
//!
//! One task runs per (listener × worker), each with its own `SO_REUSEPORT`
//! datagram socket, so the kernel pins a given client 4-tuple to a single
//! worker. The session table is therefore **worker-local and lock-free**.
//!
//! Per session we bind one upstream socket `connect(2)`-ed to the chosen
//! backend, so replies come back on the right socket without a table lookup; a
//! small task pumps that socket back to the client. A [`BackendGuard`] is held
//! for the session lifetime (so `least_conn` counts sessions and per-backend
//! caps apply), released on idle-eviction or listener shutdown.
//!
//! v0 simplifications (see `HANDOVER.md`, "Phase 2"):
//! - plain `recv_from` / `send`, not `recvmmsg` / `sendmmsg` batching;
//! - idle expiry by a 1 s sweep, not a timing wheel;
//! - one spawned reply task per session (recorded in the latency ledger);
//! - the stickiness table is bounded by a hard cap and cleared wholesale when
//!   exceeded (no LRU).
//!
//! Amplification guard: the proxy only ever sends toward a client that has an
//! established session, i.e. that sent us a datagram first.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use tokio::net::UdpSocket;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::{interval, MissedTickBehavior};

use gsp_config::{HashOn, ListenerConfig};

use crate::metrics_defs as m;
use crate::net::bind_reuseport_udp;
use crate::pool::BackendGuard;
use crate::snapshot::Snapshot;
use crate::util::now_ms;

/// Max datagram we will relay in either direction.
const MAX_DATAGRAM: usize = 64 * 1024;
/// Idle-eviction sweep cadence.
const SWEEP_PERIOD: Duration = Duration::from_secs(1);
/// Hard cap on the per-worker stickiness table; cleared wholesale when hit.
const STICKY_MAX: usize = 65_536;

struct Session {
    upstream: Arc<UdpSocket>,
    last_ms: Arc<AtomicU64>,
    backend: SocketAddr,
    /// Idle-eviction threshold, read once from the routed pool at creation.
    idle_ms: u64,
    _guard: BackendGuard,
    reply_task: JoinHandle<()>,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.reply_task.abort();
    }
}

#[derive(PartialEq, Eq, Hash)]
enum StickyKey {
    Ip(IpAddr),
    IpPort(SocketAddr),
}

fn sticky_key(affinity: Option<HashOn>, client: SocketAddr) -> Option<StickyKey> {
    match affinity {
        Some(HashOn::SrcIp) => Some(StickyKey::Ip(client.ip())),
        Some(HashOn::SrcIpPort) => Some(StickyKey::IpPort(client)),
        None => None,
    }
}

pub async fn run_udp_listener(
    cfg: ListenerConfig,
    snapshot: Arc<ArcSwap<Snapshot>>,
    worker_id: usize,
    shutdown: &mut watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let sock = Arc::new(UdpSocket::from_std(bind_reuseport_udp(cfg.bind)?)?);
    tracing::info!(
        listener = %cfg.name,
        worker = worker_id,
        bind = %cfg.bind,
        "udp listener started"
    );

    let mut sessions: HashMap<SocketAddr, Session> = HashMap::new();
    let mut sticky: HashMap<StickyKey, SocketAddr> = HashMap::new();
    let mut buf = vec![0u8; MAX_DATAGRAM];
    let mut sweep = interval(SWEEP_PERIOD);
    sweep.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    tracing::info!(listener = %cfg.name, worker = worker_id, "udp listener stopping");
                    return Ok(());
                }
            }
            _ = sweep.tick() => {
                let now = now_ms();
                let before = sessions.len();
                sessions.retain(|client, s| {
                    let alive = now.saturating_sub(s.last_ms.load(Ordering::Relaxed)) < s.idle_ms;
                    if !alive {
                        tracing::debug!(
                            listener = %cfg.name, %client, backend = %s.backend,
                            "udp session idle-evicted"
                        );
                    }
                    alive
                });
                let evicted = before - sessions.len();
                if evicted > 0 {
                    metrics::gauge!(m::ACTIVE_UDP_SESSIONS, "listener" => cfg.name.clone())
                        .decrement(evicted as f64);
                }
            }
            recv = sock.recv_from(&mut buf) => {
                let (n, client) = match recv {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(listener = %cfg.name, error = %e, "udp recv failed");
                        continue;
                    }
                };
                metrics::counter!(m::PACKETS, "listener" => cfg.name.clone(), "dir" => "c2s")
                    .increment(1);

                // Existing session: forward and refresh liveness.
                if let Some(s) = sessions.get(&client) {
                    s.last_ms.store(now_ms(), Ordering::Relaxed);
                    let up = s.upstream.clone();
                    if let Err(e) = up.send(&buf[..n]).await {
                        metrics::counter!(
                            m::DATAGRAMS_DROPPED,
                            "listener" => cfg.name.clone(), "reason" => "upstream_send",
                        ).increment(1);
                        tracing::warn!(
                            listener = %cfg.name, %client, error = %e,
                            "udp forward to backend failed"
                        );
                    }
                    continue;
                }

                // New session.
                match open_session(&cfg, &snapshot, &sock, &mut sticky, client, &buf[..n]).await {
                    Ok(session) => {
                        sessions.insert(client, session);
                        metrics::gauge!(m::ACTIVE_UDP_SESSIONS, "listener" => cfg.name.clone())
                            .increment(1.0);
                    }
                    Err(reason) => {
                        metrics::counter!(
                            m::DATAGRAMS_DROPPED,
                            "listener" => cfg.name.clone(), "reason" => reason,
                        ).increment(1);
                    }
                }
            }
        }
    }
}

/// Pick a backend (honouring stickiness), bind the upstream socket, send the
/// first datagram, and spawn the reply pump. On failure returns the
/// `gsp_datagrams_dropped_total` `reason` label to record.
async fn open_session(
    cfg: &ListenerConfig,
    snapshot: &Arc<ArcSwap<Snapshot>>,
    down: &Arc<UdpSocket>,
    sticky: &mut HashMap<StickyKey, SocketAddr>,
    client: SocketAddr,
    first: &[u8],
) -> Result<Session, &'static str> {
    let snap = snapshot.load_full();
    let local = down.local_addr().unwrap_or(cfg.bind);
    let mctx = gsp_config::MatchContext {
        src: client,
        local,
        first_bytes: first,
    };
    let pool_name = cfg.route_for(&mctx).ok_or("no_route")?;
    let pool = snap.pool(pool_name).ok_or("no_route")?;
    let idle_ms = pool.idle_timeout.as_millis() as u64;

    let skey = sticky_key(cfg.affinity, client);
    let guard = match skey
        .as_ref()
        .and_then(|k| sticky.get(k))
        .and_then(|&addr| pool.acquire_addr(addr))
    {
        Some(g) => g,
        None => pool.acquire_for(Some(client)).map_err(|e| {
            tracing::warn!(listener = %cfg.name, %client, error = %e, "no backend for udp session");
            "no_backend"
        })?,
    };
    let backend = guard.addr();

    let upstream = match connect_upstream(backend).await {
        Ok(u) => Arc::new(u),
        Err(e) => {
            guard.observe(false);
            tracing::warn!(
                listener = %cfg.name, %backend, error = %e, "udp upstream socket failed"
            );
            return Err("upstream_bind");
        }
    };
    if let Err(e) = upstream.send(first).await {
        guard.observe(false);
        tracing::warn!(listener = %cfg.name, %backend, error = %e, "udp first datagram failed");
        return Err("upstream_send");
    }
    guard.observe(true);

    if let Some(k) = skey {
        if sticky.len() >= STICKY_MAX {
            sticky.clear();
        }
        sticky.insert(k, backend);
    }

    let last_ms = Arc::new(AtomicU64::new(now_ms()));
    let reply_task = spawn_reply(
        cfg.name.clone(),
        down.clone(),
        upstream.clone(),
        client,
        last_ms.clone(),
    );

    tracing::debug!(listener = %cfg.name, %client, %backend, "udp session opened");
    Ok(Session {
        upstream,
        last_ms,
        backend,
        idle_ms,
        _guard: guard,
        reply_task,
    })
}

async fn connect_upstream(backend: SocketAddr) -> std::io::Result<UdpSocket> {
    let bind = if backend.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    let sock = UdpSocket::bind(bind).await?;
    sock.connect(backend).await?;
    Ok(sock)
}

/// Pump backend → client until the upstream socket errors (e.g. ICMP
/// port-unreachable) or the client send fails. The idle sweep reaps the
/// session entry afterwards.
fn spawn_reply(
    listener: String,
    down: Arc<UdpSocket>,
    up: Arc<UdpSocket>,
    client: SocketAddr,
    last_ms: Arc<AtomicU64>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut buf = vec![0u8; MAX_DATAGRAM];
        loop {
            match up.recv(&mut buf).await {
                Ok(n) => {
                    last_ms.store(now_ms(), Ordering::Relaxed);
                    if let Err(e) = down.send_to(&buf[..n], client).await {
                        tracing::warn!(%listener, %client, error = %e, "udp reply to client failed");
                        return;
                    }
                    metrics::counter!(m::PACKETS, "listener" => listener.clone(), "dir" => "s2c")
                        .increment(1);
                }
                Err(e) => {
                    tracing::debug!(%listener, %client, error = %e, "udp upstream recv ended");
                    return;
                }
            }
        }
    })
}
