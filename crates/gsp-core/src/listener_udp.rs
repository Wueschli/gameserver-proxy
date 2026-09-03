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
//! **Prefix mode** (`ListenerConfig::prefix`): the socket is wildcard-bound and
//! carries `IP_PKTINFO` / `IPV6_RECVPKTINFO`, so one socket serves a whole
//! routed prefix. Each datagram's real destination address is read from the
//! control message and fed to routing (the `dst` matcher); replies go back out
//! with that same address as the source (`sendmsg` + a pktinfo cmsg).
//! Datagrams to a destination outside the prefix are dropped. Sessions are then
//! keyed by `(client, dst)`. Without prefix mode nothing changes: plain
//! `recv_from` / `send_to`, sessions keyed by client only.
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
use std::io::{self, IoSlice, IoSliceMut};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use tokio::io::Interest;
use tokio::net::UdpSocket;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::{interval, MissedTickBehavior};

use gsp_config::{HashOn, ListenerConfig};

use crate::drain::{ConnGuard, ConnTracker};
use crate::limits::{GlobalLimits, LimitGuard};
use crate::metrics_defs as m;
use crate::net::{bind_reuseport_udp, bind_transparent_udp, UdpMode};
use crate::pool::BackendGuard;
use crate::ratelimit::RateLimiter;
use crate::resolver::{resolve_route, Resolvers, Routed};
use crate::route_hint::RouteHints;
use crate::snapshot::Snapshot;
use crate::util::now_ms;

/// Max datagram we will relay in either direction.
const MAX_DATAGRAM: usize = 64 * 1024;
/// Idle-eviction sweep cadence.
const SWEEP_PERIOD: Duration = Duration::from_secs(1);
/// Hard cap on the per-worker stickiness table; cleared wholesale when hit.
const STICKY_MAX: usize = 65_536;

/// Session table key: the client address, plus (in prefix / transparent mode)
/// the destination address the datagram was sent to.
type SessionKey = (SocketAddr, Option<SocketAddr>);

struct Session {
    upstream: Arc<UdpSocket>,
    last_ms: Arc<AtomicU64>,
    backend: SocketAddr,
    /// Idle-eviction threshold, read once from the routed pool at creation.
    idle_ms: u64,
    /// `None` for a resolver `target` (no pool slot to hold).
    _guard: Option<BackendGuard>,
    /// Keeps this session counted for graceful-shutdown draining.
    _conn_guard: ConnGuard,
    /// Releases the global `max_udp_sessions` slot when the session is evicted.
    _limit_guard: LimitGuard,
    /// Transparent mode: the `IP_TRANSPARENT` socket bound to the original
    /// destination address, from which replies are sent so the client sees them
    /// coming from the address it addressed. Held here to keep it alive.
    _reply_sock: Option<Arc<UdpSocket>>,
    reply_task: JoinHandle<()>,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.reply_task.abort();
    }
}

#[derive(PartialEq, Eq, Hash)]
enum Who {
    Ip(IpAddr),
    IpPort(SocketAddr),
}

#[derive(PartialEq, Eq, Hash)]
struct StickyKey {
    dst: Option<SocketAddr>,
    who: Who,
}

fn sticky_key(
    affinity: Option<HashOn>,
    client: SocketAddr,
    dst: Option<SocketAddr>,
) -> Option<StickyKey> {
    let who = match affinity? {
        HashOn::SrcIp => Who::Ip(client.ip()),
        HashOn::SrcIpPort => Who::IpPort(client),
    };
    Some(StickyKey { dst, who })
}

// Plumbing entry point: each argument is a distinct shared handle wired in by
// `ListenerManager::spawn_group` (its only caller).
#[allow(clippy::too_many_arguments)]
pub async fn run_udp_listener(
    cfg: ListenerConfig,
    snapshot: Arc<ArcSwap<Snapshot>>,
    hints: Arc<RouteHints>,
    conns: Arc<ConnTracker>,
    resolvers: Arc<Resolvers>,
    limiter: Arc<RateLimiter>,
    limits: Arc<GlobalLimits>,
    worker_id: usize,
    shutdown: &mut watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let mode = if cfg.transparent {
        UdpMode::Transparent
    } else if cfg.prefix.is_some() {
        UdpMode::Prefix
    } else {
        UdpMode::Plain
    };
    let sock = Arc::new(UdpSocket::from_std(bind_reuseport_udp(cfg.bind, mode)?)?);
    crate::sniff::warn_if_missing(&cfg.name, cfg.sniffer.as_deref());
    tracing::info!(
        listener = %cfg.name,
        worker = worker_id,
        bind = %cfg.bind,
        prefix = ?cfg.prefix,
        mode = ?mode,
        "udp listener started"
    );

    let mut sessions: HashMap<SessionKey, Session> = HashMap::new();
    let mut sticky: HashMap<StickyKey, SocketAddr> = HashMap::new();
    let mut buf = vec![0u8; MAX_DATAGRAM];
    let mut sweep = interval(SWEEP_PERIOD);
    sweep.set_missed_tick_behavior(MissedTickBehavior::Skip);

    // Once set (by the shutdown signal), no new sessions are opened; the task
    // keeps pumping existing sessions until they idle out, then returns. The
    // grace period is enforced by the caller aborting the task.
    let mut draining = false;

    loop {
        if draining && sessions.is_empty() {
            tracing::info!(listener = %cfg.name, worker = worker_id, "udp listener drained");
            return Ok(());
        }
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() && !draining {
                    draining = true;
                    tracing::info!(
                        listener = %cfg.name, worker = worker_id,
                        sessions = sessions.len(), "udp listener draining"
                    );
                }
            }
            _ = sweep.tick() => {
                let now = now_ms();
                let before = sessions.len();
                sessions.retain(|(client, _), s| {
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
            recv = recv_one(&sock, &mut buf, mode) => {
                let (n, client, dst) = match recv {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(listener = %cfg.name, error = %e, "udp recv failed");
                        continue;
                    }
                };
                metrics::counter!(m::PACKETS, "listener" => cfg.name.clone(), "dir" => "c2s")
                    .increment(1);

                // Prefix mode: drop datagrams to a destination outside the prefix
                // (and any datagram we somehow got no destination for).
                if let Some(prefix) = &cfg.prefix {
                    match dst {
                        Some(d) if prefix.contains(d.ip()) => {}
                        _ => {
                            metrics::counter!(
                                m::DATAGRAMS_DROPPED,
                                "listener" => cfg.name.clone(), "reason" => "outside_prefix",
                            ).increment(1);
                            continue;
                        }
                    }
                }

                let key: SessionKey = (client, dst);

                // Existing session: forward and refresh liveness.
                if let Some(s) = sessions.get(&key) {
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

                // Filter chain (phase 7): a new session from a denied source IP
                // is dropped before session allocation. Established sessions
                // keep their steady-state path scan-free.
                if !cfg.acl.permits(client.ip()) {
                    metrics::counter!(
                        m::FILTER_BLOCKED,
                        "listener" => cfg.name.clone(), "filter" => "acl",
                    ).increment(1);
                    continue;
                }
                if let Some(which) = limiter.permit(client.ip()) {
                    metrics::counter!(
                        m::FILTER_BLOCKED,
                        "listener" => cfg.name.clone(), "filter" => which,
                    ).increment(1);
                    continue;
                }

                // New session — refused once draining.
                if draining {
                    metrics::counter!(
                        m::DATAGRAMS_DROPPED,
                        "listener" => cfg.name.clone(), "reason" => "draining",
                    ).increment(1);
                    continue;
                }
                // Global caps (process-wide): refuse before allocating a session.
                let limit_guard = match limits.acquire_udp() {
                    Ok(g) => g,
                    Err(which) => {
                        metrics::counter!(
                            m::FILTER_BLOCKED,
                            "listener" => cfg.name.clone(), "filter" => which,
                        ).increment(1);
                        continue;
                    }
                };
                match open_session(&cfg, &snapshot, &hints, &conns, &resolvers, &sock, &mut sticky, limit_guard, client, dst, &buf[..n]).await {
                    Ok(session) => {
                        sessions.insert(key, session);
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

/// Receive one datagram. In plain mode this is `recv_from`. In prefix /
/// transparent mode it is `recvmsg` with a control message yielding the real
/// destination address the client sent to (`IP_PKTINFO` — dest IP, listener
/// port — for prefix; `IP_ORIGDSTADDR` — full dest `ip:port` — for transparent).
async fn recv_one(
    sock: &UdpSocket,
    buf: &mut [u8],
    mode: UdpMode,
) -> io::Result<(usize, SocketAddr, Option<SocketAddr>)> {
    if mode == UdpMode::Plain {
        let (n, from) = sock.recv_from(buf).await?;
        return Ok((n, from, None));
    }
    let port = sock.local_addr()?.port();
    loop {
        sock.readable().await?;
        match sock.try_io(Interest::READABLE, || recvmsg_dst(sock, &mut *buf, port)) {
            Ok(v) => return Ok(v),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(e),
        }
    }
}

fn recvmsg_dst(
    sock: &UdpSocket,
    buf: &mut [u8],
    listen_port: u16,
) -> io::Result<(usize, SocketAddr, Option<SocketAddr>)> {
    use nix::sys::socket::{recvmsg, ControlMessageOwned, MsgFlags, SockaddrStorage};

    let mut iov = [IoSliceMut::new(buf)];
    let mut cmsg = nix::cmsg_space!(nix::libc::in6_pktinfo, nix::libc::sockaddr_in6);
    let msg = recvmsg::<SockaddrStorage>(
        sock.as_raw_fd(),
        &mut iov,
        Some(&mut cmsg),
        MsgFlags::empty(),
    )
    .map_err(io::Error::from)?;

    let client = msg
        .address
        .and_then(sockaddr_to_std)
        .ok_or_else(|| io::Error::other("recvmsg returned no source address"))?;

    let mut dst = None;
    for cm in msg.cmsgs().map_err(io::Error::from)? {
        match cm {
            // Prefix mode: destination IP only; the port is the listener's.
            ControlMessageOwned::Ipv4PacketInfo(pi) => {
                let ip = Ipv4Addr::from(pi.ipi_addr.s_addr.to_ne_bytes());
                dst = Some(SocketAddr::new(IpAddr::V4(ip), listen_port));
            }
            ControlMessageOwned::Ipv6PacketInfo(pi) => {
                let ip = Ipv6Addr::from(pi.ipi6_addr.s6_addr);
                dst = Some(SocketAddr::new(IpAddr::V6(ip), listen_port));
            }
            // Transparent mode: the original destination `ip:port`.
            ControlMessageOwned::Ipv4OrigDstAddr(a) => {
                let ip = Ipv4Addr::from(a.sin_addr.s_addr.to_ne_bytes());
                dst = Some(SocketAddr::new(IpAddr::V4(ip), u16::from_be(a.sin_port)));
            }
            ControlMessageOwned::Ipv6OrigDstAddr(a) => {
                let ip = Ipv6Addr::from(a.sin6_addr.s6_addr);
                dst = Some(SocketAddr::new(IpAddr::V6(ip), u16::from_be(a.sin6_port)));
            }
            _ => {}
        }
    }
    Ok((msg.bytes, client, dst))
}

fn sockaddr_to_std(a: nix::sys::socket::SockaddrStorage) -> Option<SocketAddr> {
    use std::net::{SocketAddrV4, SocketAddrV6};
    if let Some(v4) = a.as_sockaddr_in() {
        return Some(SocketAddr::V4(SocketAddrV4::new(v4.ip(), v4.port())));
    }
    if let Some(v6) = a.as_sockaddr_in6() {
        return Some(SocketAddr::V6(SocketAddrV6::new(
            v6.ip(),
            v6.port(),
            v6.flowinfo(),
            v6.scope_id(),
        )));
    }
    None
}

/// Pick a backend (honouring stickiness), bind the upstream socket, send the
/// first datagram, and spawn the reply pump. On failure returns the
/// `gsp_datagrams_dropped_total` `reason` label to record.
#[allow(clippy::too_many_arguments)]
async fn open_session(
    cfg: &ListenerConfig,
    snapshot: &Arc<ArcSwap<Snapshot>>,
    hints: &Arc<RouteHints>,
    conns: &Arc<ConnTracker>,
    resolvers: &Arc<Resolvers>,
    down: &Arc<UdpSocket>,
    sticky: &mut HashMap<StickyKey, SocketAddr>,
    limit_guard: LimitGuard,
    client: SocketAddr,
    dst: Option<SocketAddr>,
    first: &[u8],
) -> Result<Session, &'static str> {
    let snap = snapshot.load_full();
    let local = dst.unwrap_or_else(|| down.local_addr().unwrap_or(cfg.bind));
    let hint = cfg
        .sniffer
        .as_deref()
        .and_then(crate::sniff::sniffer)
        .and_then(|s| s.sniff(first));
    let mctx = gsp_config::MatchContext {
        src: client,
        local,
        first_bytes: first,
        sniff: hint.as_ref(),
    };

    // First-packet gate (phase 7): no session unless the first datagram is
    // positively recognised. Applies before the push-resolver hint — a src_ip
    // hint must not let a spoofed flood past.
    if cfg.first_packet_gate && !cfg.first_packet_recognised(&mctx) {
        return Err("first_packet_gate");
    }

    let hinted = cfg
        .route_hint
        .then(|| hints.lookup(client.ip()))
        .flatten()
        .filter(|p| snap.pool(p).is_some());
    if hinted.is_some() {
        metrics::counter!(m::ROUTE_HINTS_APPLIED, "listener" => cfg.name.clone()).increment(1);
    }
    let routed = match hinted {
        Some(p) => Routed::Pool(p),
        None => resolve_route(cfg, resolvers, &mctx, first)
            .await
            .ok_or("no_route")?,
    };

    // Resolve the route to a concrete backend address, plus (for a pool) a
    // `BackendGuard` holding the session slot. A `target` has neither pool nor
    // guard: no health check, no cap.
    let skey = sticky_key(cfg.affinity, client, dst);
    let (backend, guard, idle_ms, proxy_protocol, pool_label) = match routed {
        Routed::Target(addr) => (
            addr,
            None,
            crate::proxy::TARGET_IDLE_TIMEOUT.as_millis() as u64,
            gsp_config::ProxyProtocol::None,
            String::new(),
        ),
        Routed::Pool(name) => {
            let pool = snap.pool(&name).ok_or("no_route")?;
            let g = match skey
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
            let addr = g.addr();
            (
                addr,
                Some(g),
                pool.idle_timeout.as_millis() as u64,
                pool.proxy_protocol,
                name,
            )
        }
    };

    // Transparent mode: bind the real client address as the upstream source so
    // the backend sees the client IP.
    let up_src = cfg.transparent.then_some(client);
    let upstream = match connect_upstream(backend, up_src).await {
        Ok(u) => Arc::new(u),
        Err(e) => {
            if let Some(g) = &guard {
                g.observe(false);
            }
            tracing::warn!(
                listener = %cfg.name, %backend, error = %e, "udp upstream socket failed"
            );
            return Err("upstream_bind");
        }
    };

    // Transparent mode: replies must appear to come from the address the client
    // originally addressed — send them from an `IP_TRANSPARENT` socket bound to
    // that `ip:port` rather than from the shared listen socket.
    let reply_sock = match (cfg.transparent, dst) {
        (true, Some(orig)) => match bind_transparent_udp(orig) {
            Ok(s) => Some(Arc::new(UdpSocket::from_std(s).map_err(|e| {
                tracing::warn!(listener = %cfg.name, %orig, error = %e, "udp reply socket failed");
                "reply_bind"
            })?)),
            Err(e) => {
                if let Some(g) = &guard {
                    g.observe(false);
                }
                tracing::warn!(listener = %cfg.name, %orig, error = %e, "udp reply socket failed");
                return Err("reply_bind");
            }
        },
        _ => None,
    };
    // v2-udp: the PROXY header is prepended to the first datagram only; every
    // later datagram of the session goes out untouched. Only `v2-udp` applies on
    // a UDP listener (config validation rejects v1/v2 here).
    let first_out: std::borrow::Cow<[u8]> = if proxy_protocol == gsp_config::ProxyProtocol::V2Udp {
        let mut hdr = crate::proxy_protocol::header(proxy_protocol, client, local);
        hdr.extend_from_slice(first);
        metrics::counter!(
            m::PROXY_PROTOCOL_HEADERS,
            "pool" => pool_label,
            "version" => proxy_protocol.label(),
        )
        .increment(1);
        std::borrow::Cow::Owned(hdr)
    } else {
        std::borrow::Cow::Borrowed(first)
    };
    if let Err(e) = upstream.send(&first_out).await {
        if let Some(g) = &guard {
            g.observe(false);
        }
        tracing::warn!(listener = %cfg.name, %backend, error = %e, "udp first datagram failed");
        return Err("upstream_send");
    }
    if let Some(g) = &guard {
        g.observe(true);
    }

    if guard.is_some() {
        if let Some(k) = skey {
            if sticky.len() >= STICKY_MAX {
                sticky.clear();
            }
            sticky.insert(k, backend);
        }
    }

    let last_ms = Arc::new(AtomicU64::new(now_ms()));
    let reply_task = spawn_reply(
        cfg.name.clone(),
        down.clone(),
        reply_sock.clone(),
        upstream.clone(),
        client,
        // Prefix mode restores the source IP via a pktinfo cmsg on the shared
        // socket; transparent mode sends from `reply_sock` and needs no cmsg.
        if reply_sock.is_some() {
            None
        } else {
            dst.map(|d| d.ip())
        },
        last_ms.clone(),
    );

    tracing::debug!(listener = %cfg.name, %client, ?dst, %backend, "udp session opened");
    Ok(Session {
        upstream,
        last_ms,
        backend,
        idle_ms,
        _guard: guard,
        _conn_guard: conns.track(),
        _limit_guard: limit_guard,
        _reply_sock: reply_sock,
        reply_task,
    })
}

async fn connect_upstream(
    backend: SocketAddr,
    source: Option<SocketAddr>,
) -> std::io::Result<UdpSocket> {
    // Transparent mode: bind the real client `ip:port` as the source with
    // `IP_TRANSPARENT` so the backend sees datagrams from the client. A
    // client/backend address-family mismatch falls back to an ordinary bind.
    if let Some(src) = source.filter(|s| s.is_ipv4() == backend.is_ipv4()) {
        let sock = UdpSocket::from_std(crate::net::bind_transparent_udp(src)?)?;
        sock.connect(backend).await?;
        return Ok(sock);
    }
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
///
/// - transparent mode: `reply_sock` is the `IP_TRANSPARENT` socket bound to the
///   original destination; replies go out with a plain `send_to`.
/// - prefix mode: `reply_src` is `Some`; the reply is sent from the shared
///   listen socket with that IP as its source (`sendmsg` + pktinfo cmsg).
/// - plain mode: neither is set; a plain `send_to` on the shared socket.
fn spawn_reply(
    listener: String,
    down: Arc<UdpSocket>,
    reply_sock: Option<Arc<UdpSocket>>,
    up: Arc<UdpSocket>,
    client: SocketAddr,
    reply_src: Option<IpAddr>,
    last_ms: Arc<AtomicU64>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let out = reply_sock.as_deref().unwrap_or(down.as_ref());
        let mut buf = vec![0u8; MAX_DATAGRAM];
        loop {
            match up.recv(&mut buf).await {
                Ok(n) => {
                    last_ms.store(now_ms(), Ordering::Relaxed);
                    let sent = if reply_sock.is_some() {
                        out.send_to(&buf[..n], client).await
                    } else {
                        send_reply(out, &buf[..n], client, reply_src).await
                    };
                    if let Err(e) = sent {
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

async fn send_reply(
    sock: &UdpSocket,
    data: &[u8],
    client: SocketAddr,
    src: Option<IpAddr>,
) -> io::Result<usize> {
    let Some(src) = src else {
        return sock.send_to(data, client).await;
    };
    loop {
        sock.writable().await?;
        match sock.try_io(Interest::WRITABLE, || {
            sendmsg_pktinfo(sock, data, client, src)
        }) {
            Ok(n) => return Ok(n),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(e),
        }
    }
}

fn sendmsg_pktinfo(
    sock: &UdpSocket,
    data: &[u8],
    client: SocketAddr,
    src: IpAddr,
) -> io::Result<usize> {
    use nix::sys::socket::{sendmsg, ControlMessage, MsgFlags, SockaddrStorage};

    let iov = [IoSlice::new(data)];
    let dest = SockaddrStorage::from(client);

    let n = match src {
        IpAddr::V4(v4) => {
            let pi = nix::libc::in_pktinfo {
                ipi_ifindex: 0,
                ipi_spec_dst: nix::libc::in_addr {
                    s_addr: u32::from_ne_bytes(v4.octets()),
                },
                ipi_addr: nix::libc::in_addr { s_addr: 0 },
            };
            sendmsg::<SockaddrStorage>(
                sock.as_raw_fd(),
                &iov,
                &[ControlMessage::Ipv4PacketInfo(&pi)],
                MsgFlags::empty(),
                Some(&dest),
            )
        }
        IpAddr::V6(v6) => {
            let pi = nix::libc::in6_pktinfo {
                ipi6_addr: nix::libc::in6_addr {
                    s6_addr: v6.octets(),
                },
                ipi6_ifindex: 0,
            };
            sendmsg::<SockaddrStorage>(
                sock.as_raw_fd(),
                &iov,
                &[ControlMessage::Ipv6PacketInfo(&pi)],
                MsgFlags::empty(),
                Some(&dest),
            )
        }
    }
    .map_err(io::Error::from)?;
    Ok(n)
}
