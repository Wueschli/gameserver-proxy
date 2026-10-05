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
//! The ingress path batches: one `recvmmsg(2)` on Linux pulls up to
//! [`RECV_BATCH`] datagrams per wakeup (see [`RecvBatch`]); non-Linux and the
//! per-session reply pump still do one datagram per syscall.
//!
//! Idle expiry is a single-level timing wheel ([`IdleWheel`], 1 s slots) —
//! O(slot) work per tick instead of an O(sessions) scan.
//!
//! v0 simplifications still open (see `HANDOVER.md`, "Phase 2"):
//! - the reply pump and the upstream forward are not `sendmmsg`-batched;
//! - one spawned reply task per session (recorded in the latency ledger);
//! - the stickiness table is bounded by a hard cap and cleared wholesale when
//!   exceeded (no LRU).
//!
//! Amplification guard: the proxy only ever sends toward a client that has an
//! established session, i.e. that sent us a datagram first.

use std::cell::RefCell;
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
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{interval, MissedTickBehavior};

use gsp_config::{Action, ListenerConfig, RouteHint};

use crate::drain::{ConnGuard, ConnTracker};
use crate::error::ListenerError;
use crate::geo::GeoDb;
use crate::limits::{GlobalLimits, LimitGuard};
use crate::metrics_defs as m;
use crate::net::{bind_reuseport_udp, bind_transparent_udp, UdpMode};
use crate::pool::{Backend, BackendGuard};
use crate::ratelimit::RateLimiter;
use crate::resolver::{resolve_route, Resolvers, Routed};
use crate::route_hint::RouteHints;
use crate::snapshot::Snapshot;
use crate::sniff::Sniffers;
use crate::src_conns::{SourceGuard, SourceLimiter};
use crate::util::mono_ms;

/// Max datagram we will relay in either direction.
const MAX_DATAGRAM: usize = 64 * 1024;
/// Idle-eviction timing-wheel tick cadence (also the eviction granularity).
const WHEEL_TICK: Duration = Duration::from_secs(1);
/// Max new sessions per worker whose external resolver call is still in flight.
/// Past it, first datagrams are dropped (`pending_full`): a resolver outage or a
/// spoofed-source flood cannot grow the buffered state without bound.
const PENDING_MAX: usize = 1024;
/// Max datagrams buffered per pending session (the first, plus a few that
/// arrive before the route is known); later ones are dropped (`pending_full`).
const PENDING_PACKETS: usize = 4;
/// Max total bytes buffered across a worker's pending sessions (worst case would
/// otherwise be `PENDING_MAX` × `PENDING_PACKETS` × 64 KiB). Datagrams past it
/// are dropped (`pending_full`).
const PENDING_BYTES_MAX: usize = 1024 * 1024;

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
    /// Backend handle for passive health: a connected UDP socket that draws an
    /// ICMP port-unreachable reports `ConnectionRefused` on send/recv, which we
    /// feed to the health streaks. `None` for a resolver `target`.
    health: Option<Arc<Backend>>,
    /// Keeps this session counted for graceful-shutdown draining.
    _conn_guard: ConnGuard,
    /// Releases the global `max_udp_sessions` slot when the session is evicted.
    _limit_guard: LimitGuard,
    /// Releases the per-source concurrent-session slot when the session is evicted.
    _src_guard: SourceGuard,
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

/// A new session whose route is being resolved off the receive loop. Holds the
/// session-cap guards so a pending session counts against the same limits.
struct Pending {
    /// Datagrams received so far, in order; the first goes out with the
    /// session's PROXY header, the rest are flushed after it.
    packets: Vec<Vec<u8>>,
    src_guard: SourceGuard,
    limit_guard: LimitGuard,
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
    src_limiter: Arc<SourceLimiter>,
    limits: Arc<GlobalLimits>,
    geo: Option<Arc<GeoDb>>,
    sniffers: Arc<Sniffers>,
    worker_id: usize,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<(), ListenerError> {
    let mode = if cfg.transparent {
        UdpMode::Transparent
    } else if cfg.prefix.is_some() {
        UdpMode::Prefix
    } else {
        UdpMode::Plain
    };
    let sock = Arc::new(UdpSocket::from_std(bind_reuseport_udp(cfg.bind, mode)?)?);
    crate::sniff::warn_if_missing(&cfg.name, &cfg.sniffers, &sniffers);
    tracing::info!(
        listener = %cfg.name,
        worker = worker_id,
        bind = %cfg.bind,
        prefix = ?cfg.prefix,
        mode = ?mode,
        "udp listener started"
    );

    let cfg = Arc::new(cfg);
    let mut sessions: HashMap<SessionKey, Session> = HashMap::new();
    let mut pending: HashMap<SessionKey, Pending> = HashMap::new();
    let mut pending_bytes = 0usize;
    // One task per pending session; dropping the set (listener exit) aborts them.
    let mut resolving: JoinSet<(SessionKey, Option<Routed>)> = JoinSet::new();
    let mut rbatch = RecvBatch::new();
    let mut wheel = IdleWheel::new();
    let mut wheel_tick = interval(WHEEL_TICK);
    wheel_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // Resolve the per-batch counter once: a registry lookup plus a `String`
    // clone per received batch is pure overhead on the hot path.
    let packets_c2s = metrics::counter!(m::PACKETS, "listener" => cfg.name.clone(), "dir" => "c2s");

    // Once set (by the shutdown signal), no new sessions are opened; the task
    // keeps pumping existing sessions until they idle out, then returns. The
    // grace period is enforced by the caller aborting the task.
    let mut draining = false;

    loop {
        if draining && sessions.is_empty() && pending.is_empty() {
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
            _ = wheel_tick.tick() => {
                let evicted = wheel.tick(&cfg.name, mono_ms(), &mut sessions);
                if evicted > 0 {
                    metrics::gauge!(m::ACTIVE_UDP_SESSIONS, "listener" => cfg.name.clone())
                        .decrement(evicted as f64);
                }
            }
            Some(done) = resolving.join_next(), if !resolving.is_empty() => {
                let (key, routed) = match done {
                    Ok(d) => d,
                    Err(e) => {
                        // A resolver task panicked: its key is lost, so release
                        // every pending slot rather than leak them.
                        tracing::error!(listener = %cfg.name, error = %e, "udp resolve task failed");
                        pending.clear();
                        pending_bytes = 0;
                        continue;
                    }
                };
                let Some(p) = pending.remove(&key) else { continue };
                let dropped = p.packets.len() as u64;
                pending_bytes -= p.packets.iter().map(Vec::len).sum::<usize>();
                let reason = if draining {
                    Err("draining")
                } else {
                    routed.ok_or("no_route")
                };
                let session = match reason {
                    Ok(routed) => {
                        open_session(
                            &cfg, &snapshot, &conns, &sock, p.src_guard,
                            p.limit_guard, key.0, key.1, routed, &p.packets[0],
                        ).await
                    }
                    Err(r) => Err(r),
                };
                match session {
                    Ok(session) => {
                        for extra in &p.packets[1..] {
                            if let Err(e) = session.upstream.send(extra).await {
                                note_port_unreachable(&cfg.name, &session.health, &e);
                                metrics::counter!(
                                    m::DATAGRAMS_DROPPED,
                                    "listener" => cfg.name.clone(), "reason" => "upstream_send",
                                ).increment(1);
                            }
                        }
                        let now = mono_ms();
                        wheel.schedule(key, now + session.idle_ms, now);
                        sessions.insert(key, session);
                        metrics::gauge!(m::ACTIVE_UDP_SESSIONS, "listener" => cfg.name.clone())
                            .increment(1.0);
                    }
                    Err(reason) => {
                        metrics::counter!(
                            m::DATAGRAMS_DROPPED,
                            "listener" => cfg.name.clone(), "reason" => reason,
                        ).increment(dropped);
                    }
                }
            }
            recv = rbatch.recv(&sock, mode) => {
                let count = match recv {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!(listener = %cfg.name, error = %e, "udp recv failed");
                        continue;
                    }
                };
                if count > 0 {
                    packets_c2s.increment(count as u64);
                }
                // One `recvmmsg` (Linux) pulled up to `RECV_BATCH` datagrams;
                // route / forward each. `continue` skips to the next datagram.
                for i in 0..count {
                    let (data, client, dst) = rbatch.at(i);

                    // Prefix mode: drop datagrams to a destination outside the
                    // prefix (and any datagram we somehow got no destination for).
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
                        s.last_ms.store(mono_ms(), Ordering::Relaxed);
                        let up = s.upstream.clone();
                        if let Err(e) = up.send(data).await {
                            note_port_unreachable(&cfg.name, &s.health, &e);
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

                    // Route still resolving: buffer behind the first datagram.
                    if let Some(p) = pending.get_mut(&key) {
                        if p.packets.len() < PENDING_PACKETS
                            && pending_bytes + data.len() <= PENDING_BYTES_MAX
                        {
                            pending_bytes += data.len();
                            p.packets.push(data.to_vec());
                        } else {
                            metrics::counter!(
                                m::DATAGRAMS_DROPPED,
                                "listener" => cfg.name.clone(), "reason" => "pending_full",
                            ).increment(1);
                        }
                        continue;
                    }

                    // Filter chain (phase 7): a new session from a denied source
                    // IP is dropped before session allocation. Established
                    // sessions keep their steady-state path scan-free.
                    if !cfg.acl.permits(client.ip()) {
                        metrics::counter!(
                            m::FILTER_BLOCKED,
                            "listener" => cfg.name.clone(), "filter" => "acl",
                        ).increment(1);
                        continue;
                    }
                    if let Some(geo_acl) = &cfg.geo {
                        let admitted = geo
                            .as_deref()
                            .map(|db| geo_acl.permits(db.country_code(client.ip())))
                            .unwrap_or(false); // fail closed if the DB did not load
                        if !admitted {
                            metrics::counter!(
                                m::FILTER_BLOCKED,
                                "listener" => cfg.name.clone(), "filter" => "geo",
                            ).increment(1);
                            continue;
                        }
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
                    // Per-source concurrent cap (per listener).
                    let src_guard = match src_limiter.acquire(client.ip()) {
                        Ok(g) => g,
                        Err(which) => {
                            metrics::counter!(
                                m::FILTER_BLOCKED,
                                "listener" => cfg.name.clone(), "filter" => which,
                            ).increment(1);
                            continue;
                        }
                    };
                    // Global caps (process-wide): refuse before allocating.
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
                    let local = local_addr(&cfg, &sock, dst);
                    let routed = match prepare_route(&cfg, &snapshot, &hints, &sniffers, client, local, data) {
                        Ok(Prepared::Routed(r)) => r,
                        Ok(Prepared::Resolve(hit)) => {
                            // Resolve off the receive loop so a slow resolver
                            // never stalls the other sessions on this worker.
                            if pending.len() >= PENDING_MAX
                                || pending_bytes + data.len() > PENDING_BYTES_MAX
                            {
                                metrics::counter!(
                                    m::DATAGRAMS_DROPPED,
                                    "listener" => cfg.name.clone(), "reason" => "pending_full",
                                ).increment(1);
                                continue;
                            }
                            pending_bytes += data.len();
                            let first = data.to_vec();
                            pending.insert(key, Pending {
                                packets: vec![first.clone()],
                                src_guard,
                                limit_guard,
                            });
                            let (cfg, resolvers) = (cfg.clone(), resolvers.clone());
                            resolving.spawn(async move {
                                let routed = resolve_pending(
                                    &cfg, &resolvers, client, local, &first, hit.as_ref(),
                                ).await;
                                (key, routed)
                            });
                            continue;
                        }
                        Err(reason) => {
                            metrics::counter!(
                                m::DATAGRAMS_DROPPED,
                                "listener" => cfg.name.clone(), "reason" => reason,
                            ).increment(1);
                            continue;
                        }
                    };
                    match open_session(&cfg, &snapshot, &conns, &sock, src_guard, limit_guard, client, dst, routed, data).await {
                        Ok(session) => {
                            let now = mono_ms();
                            wheel.schedule(key, now + session.idle_ms, now);
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
}

/// `recvmsg` with a control message yielding the real destination address the
/// client sent to (`IP_PKTINFO` — dest IP, listener port — for prefix;
/// `IP_ORIGDSTADDR` — full dest `ip:port` — for transparent). The Linux ingress
/// path uses `recvmmsg` in [`RecvBatch`]; this is the non-Linux fallback and the
/// shared cmsg-parsing reference.
#[cfg_attr(target_os = "linux", allow(dead_code))]
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

/// Max datagrams pulled from the listen socket in one `recvmmsg` (Linux).
const RECV_BATCH: usize = 16;

/// One received datagram's metadata (the payload stays in [`RecvBatch`]'s buffers).
struct Datagram {
    len: usize,
    client: SocketAddr,
    /// Real destination from the pktinfo / origdst cmsg (prefix / transparent).
    dst: Option<SocketAddr>,
}

/// Reusable receive buffers for the listen socket's ingress path.
///
/// On Linux one [`RecvBatch::recv`] pulls up to [`RECV_BATCH`] datagrams with a
/// single `recvmmsg(2)` — the per-datagram routing / forwarding work then runs
/// over the batch, amortising the receive syscall. Elsewhere it degrades to one
/// `recvmsg` per call. Either way the caller reads payload `i` via
/// [`RecvBatch::at`].
struct RecvBatch {
    bufs: Vec<Vec<u8>>,
    meta: Vec<Datagram>,
}

impl RecvBatch {
    fn new() -> Self {
        let cap = if cfg!(target_os = "linux") {
            RECV_BATCH
        } else {
            1
        };
        Self {
            bufs: (0..cap).map(|_| vec![0u8; MAX_DATAGRAM]).collect(),
            meta: Vec::with_capacity(cap),
        }
    }

    /// Payload and addressing of the `i`-th datagram from the last `recv`.
    fn at(&self, i: usize) -> (&[u8], SocketAddr, Option<SocketAddr>) {
        let d = &self.meta[i];
        (&self.bufs[i][..d.len], d.client, d.dst)
    }

    /// Await readability and receive a batch; returns how many datagrams landed.
    async fn recv(&mut self, sock: &UdpSocket, mode: UdpMode) -> io::Result<usize> {
        let port = if mode == UdpMode::Plain {
            0
        } else {
            sock.local_addr()?.port()
        };
        loop {
            sock.readable().await?;
            match sock.try_io(Interest::READABLE, || self.recv_now(sock, mode, port)) {
                Ok(n) => return Ok(n),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Err(e) => return Err(e),
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn recv_now(&mut self, sock: &UdpSocket, mode: UdpMode, port: u16) -> io::Result<usize> {
        self.meta.clear();
        let (len, client, dst) = if mode == UdpMode::Plain {
            let (n, from) = sock.try_recv_from(&mut self.bufs[0])?;
            (n, from, None)
        } else {
            recvmsg_dst(sock, &mut self.bufs[0], port)?
        };
        self.meta.push(Datagram { len, client, dst });
        Ok(1)
    }

    #[cfg(target_os = "linux")]
    fn recv_now(&mut self, sock: &UdpSocket, mode: UdpMode, port: u16) -> io::Result<usize> {
        use nix::sys::socket::{recvmmsg, MsgFlags, MultiHeaders, SockaddrStorage};

        let Self { bufs, meta } = self;
        meta.clear();
        let want_cmsg = mode != UdpMode::Plain;
        let cmsg =
            want_cmsg.then(|| nix::cmsg_space!(nix::libc::in6_pktinfo, nix::libc::sockaddr_in6));
        // A fresh header set per call: `recvmmsg` does not reset `msg_namelen` /
        // `msg_controllen` between calls, so a reused set would truncate an
        // address / cmsg after the first differing datagram.
        let mut headers = MultiHeaders::<SockaddrStorage>::preallocate(bufs.len(), cmsg);
        let mut iovs: Vec<[IoSliceMut<'_>; 1]> = bufs
            .iter_mut()
            .map(|b| [IoSliceMut::new(b.as_mut_slice())])
            .collect();

        let results = recvmmsg(
            sock.as_raw_fd(),
            &mut headers,
            iovs.iter_mut(),
            MsgFlags::MSG_DONTWAIT,
            None::<nix::sys::time::TimeSpec>,
        )
        .map_err(io::Error::from)?;

        for msg in results {
            // No source address: drop this slot.
            let Some(client) = msg.address.and_then(sockaddr_to_std) else {
                continue;
            };
            let mut dst = None;
            if want_cmsg {
                for cm in msg.cmsgs().map_err(io::Error::from)? {
                    if let Some(d) = dst_from_cmsg(cm, port) {
                        dst = Some(d);
                    }
                }
            }
            meta.push(Datagram {
                len: msg.bytes,
                client,
                dst,
            });
        }
        Ok(meta.len())
    }
}

/// Turn a pktinfo / origdst control message into the destination `SocketAddr`.
/// `listen_port` fills the port for `IP_PKTINFO` (which carries only the IP).
#[cfg(target_os = "linux")]
#[allow(clippy::needless_pass_by_value)] // `ControlMessageOwned` is consumed by value everywhere else it is matched
fn dst_from_cmsg(
    cm: nix::sys::socket::ControlMessageOwned,
    listen_port: u16,
) -> Option<SocketAddr> {
    use nix::sys::socket::ControlMessageOwned as C;
    match cm {
        C::Ipv4PacketInfo(pi) => {
            let ip = Ipv4Addr::from(pi.ipi_addr.s_addr.to_ne_bytes());
            Some(SocketAddr::new(IpAddr::V4(ip), listen_port))
        }
        C::Ipv6PacketInfo(pi) => {
            let ip = Ipv6Addr::from(pi.ipi6_addr.s6_addr);
            Some(SocketAddr::new(IpAddr::V6(ip), listen_port))
        }
        C::Ipv4OrigDstAddr(a) => {
            let ip = Ipv4Addr::from(a.sin_addr.s_addr.to_ne_bytes());
            Some(SocketAddr::new(IpAddr::V4(ip), u16::from_be(a.sin_port)))
        }
        C::Ipv6OrigDstAddr(a) => {
            let ip = Ipv6Addr::from(a.sin6_addr.s6_addr);
            Some(SocketAddr::new(IpAddr::V6(ip), u16::from_be(a.sin6_port)))
        }
        _ => None,
    }
}

/// Number of 1-second slots in [`IdleWheel`]. A session whose idle timeout
/// exceeds this span is simply re-checked every `WHEEL_SLOTS` seconds until it
/// actually expires.
const WHEEL_SLOTS: usize = 512;

/// A single-level timing wheel for UDP session idle expiry, replacing an
/// O(sessions) `retain` scan every second with O(slot) work.
///
/// Each session is filed in the slot for the second its idle deadline falls in.
/// A tick advances the hand one slot and drains it: an entry whose session's
/// live `last_ms` shows it is genuinely idle is evicted; one that a datagram
/// refreshed (or whose `idle_ms` outran the wheel span) is re-filed at its new
/// deadline. The per-datagram hot path is untouched — it still only bumps the
/// atomic `last_ms`. Exactly one wheel entry exists per live session.
struct IdleWheel {
    slots: Vec<Vec<SessionKey>>,
    hand: usize,
}

impl IdleWheel {
    fn new() -> Self {
        Self {
            slots: (0..WHEEL_SLOTS).map(|_| Vec::new()).collect(),
            hand: 0,
        }
    }

    /// File `key` in the slot for `deadline_ms`, at least one slot ahead so it is
    /// never processed on the current tick.
    fn schedule(&mut self, key: SessionKey, deadline_ms: u64, now: u64) {
        let secs_ahead = (deadline_ms.saturating_sub(now) / 1000) as usize;
        let ahead = secs_ahead.clamp(1, WHEEL_SLOTS - 1);
        let slot = (self.hand + ahead) % WHEEL_SLOTS;
        self.slots[slot].push(key);
    }

    /// Advance one slot; evict genuinely-idle sessions, re-file the rest. Returns
    /// the number evicted (for the `ACTIVE_UDP_SESSIONS` gauge).
    fn tick(
        &mut self,
        listener: &str,
        now: u64,
        sessions: &mut HashMap<SessionKey, Session>,
    ) -> usize {
        self.hand = (self.hand + 1) % WHEEL_SLOTS;
        let due = std::mem::take(&mut self.slots[self.hand]);
        let mut evicted = 0;
        for key in due {
            let Some(s) = sessions.get(&key) else {
                continue; // session already gone by another path
            };
            let deadline = s.last_ms.load(Ordering::Relaxed) + s.idle_ms;
            if now >= deadline {
                tracing::debug!(
                    listener = %listener, client = %key.0, backend = %s.backend,
                    "udp session idle-evicted"
                );
                sessions.remove(&key);
                evicted += 1;
            } else {
                self.schedule(key, deadline, now);
            }
        }
        evicted
    }
}

/// Outcome of the synchronous half of routing a new session.
enum Prepared {
    /// Route known without any await (hint, or the first matching route is a
    /// static pool).
    Routed(Routed),
    /// The first matching route is an external resolver: the call must run off
    /// the receive loop. Carries the sniffer hit for the resolve request.
    Resolve(Option<(String, RouteHint)>),
}

/// The destination address this datagram was sent to (for a plain listener, the
/// bound address).
fn local_addr(cfg: &ListenerConfig, down: &UdpSocket, dst: Option<SocketAddr>) -> SocketAddr {
    dst.unwrap_or_else(|| down.local_addr().unwrap_or(cfg.bind))
}

/// Everything that decides a new session's route without awaiting: sniffer,
/// first-packet gate, push-resolver hint, and the route walk up to its first
/// match. Never touches the network, so it is safe on the receive loop. On
/// failure returns the `gsp_datagrams_dropped_total` `reason` label.
fn prepare_route(
    cfg: &ListenerConfig,
    snapshot: &Arc<ArcSwap<Snapshot>>,
    hints: &Arc<RouteHints>,
    sniffers: &Arc<Sniffers>,
    client: SocketAddr,
    local: SocketAddr,
    first: &[u8],
) -> Result<Prepared, &'static str> {
    let snap = snapshot.load_full();
    let hit = crate::sniff::sniff_first(&cfg.sniffers, sniffers, first);
    let mctx = gsp_config::MatchContext {
        src: client,
        local,
        first_bytes: first,
        sniff: hit.as_ref().map(|(n, h)| (*n, h)),
    };

    // A sniffer that positively rejects drops the datagram outright — no
    // session, no reply (amplifier-safe). Before the gate / push-resolver hint:
    // a content-based reject outranks a spoofable src_ip hint.
    if hit.as_ref().is_some_and(|(_, h)| h.reject) {
        return Err("sniffer_reject");
    }

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
    if let Some(p) = hinted {
        metrics::counter!(m::ROUTE_HINTS_APPLIED, "listener" => cfg.name.clone()).increment(1);
        return Ok(Prepared::Routed(Routed::Pool(p)));
    }
    // The walk ends at the first matching `Pool` and only reaches the next route
    // after a resolver call, so the first match alone says whether to await.
    let first_action = cfg.matching_routes(&mctx).next().map(|r| r.action.clone());
    match first_action {
        None => Err("no_route"),
        Some(Action::Pool(p)) => Ok(Prepared::Routed(Routed::Pool(p))),
        Some(Action::Resolver(_)) => Ok(Prepared::Resolve(hit.map(|(n, h)| (n.to_string(), h)))),
    }
}

/// Run the full route walk, including the external resolver call. Called from a
/// spawned task, never from the receive loop.
async fn resolve_pending(
    cfg: &ListenerConfig,
    resolvers: &Resolvers,
    client: SocketAddr,
    local: SocketAddr,
    first: &[u8],
    hit: Option<&(String, RouteHint)>,
) -> Option<Routed> {
    let mctx = gsp_config::MatchContext {
        src: client,
        local,
        first_bytes: first,
        sniff: hit.map(|(n, h)| (n.as_str(), h)),
    };
    resolve_route(cfg, resolvers, &mctx, first).await
}

/// Pick a backend (affinity comes from the pool's balancer), bind the upstream
/// socket, send the first datagram, and spawn the reply pump, for an
/// already-routed session. On failure returns the `gsp_datagrams_dropped_total`
/// `reason` label to record.
#[allow(clippy::too_many_arguments)]
async fn open_session(
    cfg: &ListenerConfig,
    snapshot: &Arc<ArcSwap<Snapshot>>,
    conns: &Arc<ConnTracker>,
    down: &Arc<UdpSocket>,
    src_guard: SourceGuard,
    limit_guard: LimitGuard,
    client: SocketAddr,
    dst: Option<SocketAddr>,
    routed: Routed,
    first: &[u8],
) -> Result<Session, &'static str> {
    let snap = snapshot.load_full();
    let local = local_addr(cfg, down, dst);

    // Resolve the route to a concrete backend address, plus (for a pool) a
    // `BackendGuard` holding the session slot. A `target` has neither pool nor
    // guard: no health check, no cap.
    let (backend, guard, idle_ms, proxy_protocol, pool_label) = match routed {
        Routed::Target {
            addr,
            proxy_protocol,
            // no connect(2) handshake on a UDP upstream socket
            connect_timeout: _,
            idle_timeout,
        } => (
            addr,
            None,
            idle_timeout.as_millis() as u64,
            proxy_protocol,
            "(resolver target)".to_string(),
        ),
        Routed::Pool(name) => {
            let pool = snap.pool(&name).ok_or("no_route")?;
            let g = pool.acquire_for(Some(client)).map_err(|e| {
                tracing::warn!(listener = %cfg.name, %client, error = %e, "no backend for udp session");
                "no_backend"
            })?;
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
            "pool" => pool_label.clone(),
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

    let health = guard.as_ref().map(super::pool::BackendGuard::backend);
    let last_ms = Arc::new(AtomicU64::new(mono_ms()));
    let reply_task = spawn_reply(
        cfg.name.clone(),
        down.clone(),
        reply_sock.clone(),
        upstream.clone(),
        health.clone(),
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
    let conn_guard = conns.track(crate::drain::SessionMeta {
        proto: crate::drain::Proto::Udp,
        listener: cfg.name.clone(),
        peer: client,
        local,
    });
    conn_guard.set_target(Some(pool_label.as_str()), backend);
    Ok(Session {
        upstream,
        last_ms,
        backend,
        idle_ms,
        health,
        _guard: guard,
        _conn_guard: conn_guard,
        _limit_guard: limit_guard,
        _src_guard: src_guard,
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

/// Feed a passive unhealthy observation when a connected upstream UDP socket
/// reports `ConnectionRefused` (on Linux, the ICMP port-unreachable the backend
/// host sends when nothing is listening). Other errors are ignored, and a
/// resolver `target` has no backend to mark; the idle sweep reaps the session
/// either way.
fn note_port_unreachable(listener: &str, health: &Option<Arc<Backend>>, err: &io::Error) {
    if err.kind() != io::ErrorKind::ConnectionRefused {
        return;
    }
    if let Some(b) = health {
        if let Some(state) = b.observe(false) {
            tracing::info!(
                listener = %listener, backend = %b.addr, healthy = state,
                "backend health changed (passive, udp port-unreachable)"
            );
        }
    }
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
#[allow(clippy::too_many_arguments)]
fn spawn_reply(
    listener: String,
    down: Arc<UdpSocket>,
    reply_sock: Option<Arc<UdpSocket>>,
    up: Arc<UdpSocket>,
    health: Option<Arc<Backend>>,
    client: SocketAddr,
    reply_src: Option<IpAddr>,
    last_ms: Arc<AtomicU64>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let out = reply_sock.as_deref().unwrap_or(down.as_ref());
        // Once per session, not per reply packet.
        let packets_s2c =
            metrics::counter!(m::PACKETS, "listener" => listener.clone(), "dir" => "s2c");
        loop {
            // Receive and forward inside one synchronous section so a single
            // per-thread buffer serves every session (#122): a buffer per
            // session cost 64 KiB each and stayed resident after a burst. The
            // closure goes through `async_io` with `ERROR` interest, as
            // tokio's own `recv` does: an ICMP port-unreachable is a socket error
            // with no data and must wake the pump.
            let relayed = up
                .async_io(Interest::READABLE | Interest::ERROR, || {
                    REPLY_BUF.with_borrow_mut(|buf| {
                        // Raw recv, not `try_recv`: that re-checks readiness and
                        // would skip the syscall for an error-only wakeup.
                        let n = nix::sys::socket::recv(
                            up.as_raw_fd(),
                            buf,
                            nix::sys::socket::MsgFlags::MSG_DONTWAIT,
                        )
                        .map_err(io::Error::from)?;
                        Ok(match try_send_reply(out, &buf[..n], client, reply_src) {
                            Ok(_) => Relay::Sent,
                            // Client socket full: park a right-sized copy and
                            // await writability outside the borrow.
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                                Relay::Blocked(buf[..n].to_vec())
                            }
                            Err(e) => Relay::Failed(e),
                        })
                    })
                })
                .await;
            let sent = match relayed {
                Ok(Relay::Sent) => Ok(()),
                Ok(Relay::Blocked(data)) => {
                    send_reply(out, &data, client, reply_src).await.map(drop)
                }
                Ok(Relay::Failed(e)) => Err(e),
                Err(e) => {
                    note_port_unreachable(&listener, &health, &e);
                    tracing::debug!(%listener, %client, error = %e, "udp upstream recv ended");
                    return;
                }
            };
            last_ms.store(mono_ms(), Ordering::Relaxed);
            if let Err(e) = sent {
                tracing::warn!(%listener, %client, error = %e, "udp reply to client failed");
                return;
            }
            packets_s2c.increment(1);
        }
    })
}

thread_local! {
    /// Scratch buffer for the reply pumps of every session on this thread; only
    /// ever borrowed inside a synchronous section, never across an `.await`.
    static REPLY_BUF: RefCell<Vec<u8>> = RefCell::new(vec![0u8; MAX_DATAGRAM]);
}

enum Relay {
    Sent,
    /// The datagram, to send once the client socket is writable.
    Blocked(Vec<u8>),
    /// Sending to the client failed.
    Failed(io::Error),
}

/// Non-blocking [`send_reply`]: `WouldBlock` if the socket's send buffer is full.
fn try_send_reply(
    sock: &UdpSocket,
    data: &[u8],
    client: SocketAddr,
    src: Option<IpAddr>,
) -> io::Result<usize> {
    match src {
        None => sock.try_send_to(data, client),
        Some(src) => sock.try_io(Interest::WRITABLE, || {
            sendmsg_pktinfo(sock, data, client, src)
        }),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drain::{Proto, SessionMeta};

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    /// A minimal live session for wheel tests: no pool slot, no limits.
    async fn session(client: SocketAddr, last_ms: u64, idle_ms: u64) -> Session {
        session_tracked(&ConnTracker::new(), client, last_ms, idle_ms).await
    }

    async fn session_tracked(
        tracker: &Arc<ConnTracker>,
        client: SocketAddr,
        last_ms: u64,
        idle_ms: u64,
    ) -> Session {
        let upstream = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let limits = GlobalLimits::new(&gsp_config::GlobalLimits::default());
        let src = SourceLimiter::new(None);
        Session {
            upstream,
            last_ms: Arc::new(AtomicU64::new(last_ms)),
            backend: addr("127.0.0.1:9"),
            idle_ms,
            _guard: None,
            health: None,
            _conn_guard: tracker.track(SessionMeta {
                proto: Proto::Udp,
                listener: "t".into(),
                peer: client,
                local: addr("127.0.0.1:1"),
            }),
            _limit_guard: limits.acquire_udp().unwrap(),
            _src_guard: src.acquire(client.ip()).unwrap(),
            _reply_sock: None,
            reply_task: tokio::spawn(async {}),
        }
    }

    fn slot_of(w: &IdleWheel, key: &SessionKey) -> Option<usize> {
        w.slots.iter().position(|s| s.contains(key))
    }

    #[test]
    fn sockaddr_to_std_round_trips_v4_and_v6() {
        use nix::sys::socket::SockaddrStorage;
        for a in [addr("192.0.2.7:4242"), addr("[2001:db8::7]:4242")] {
            assert_eq!(sockaddr_to_std(SockaddrStorage::from(a)), Some(a));
        }
    }

    #[test]
    fn schedule_files_at_least_one_slot_ahead() {
        let mut w = IdleWheel::new();
        let k: SessionKey = (addr("10.0.0.1:1"), None);
        // Deadline already passed: must still not land on the current hand.
        w.schedule(k, 1_000, 5_000);
        assert_eq!(slot_of(&w, &k), Some(1));
    }

    #[test]
    fn schedule_rounds_down_to_whole_seconds() {
        let mut w = IdleWheel::new();
        let k: SessionKey = (addr("10.0.0.1:1"), None);
        w.schedule(k, 10_000 + 3_999, 10_000);
        assert_eq!(slot_of(&w, &k), Some(3));
    }

    #[test]
    fn schedule_clamps_long_deadlines_inside_the_wheel() {
        let mut w = IdleWheel::new();
        w.hand = WHEEL_SLOTS - 2;
        let k: SessionKey = (addr("10.0.0.1:1"), None);
        // Far beyond the wheel span: clamped to WHEEL_SLOTS - 1, wrapping round.
        w.schedule(k, 10_000_000, 0);
        let expect = (WHEEL_SLOTS - 2 + WHEEL_SLOTS - 1) % WHEEL_SLOTS;
        assert_eq!(slot_of(&w, &k), Some(expect));
        assert_ne!(expect, w.hand, "clamped entry must not alias the hand");
    }

    #[test]
    fn tick_skips_sessions_already_gone() {
        let mut w = IdleWheel::new();
        let k: SessionKey = (addr("10.0.0.1:1"), None);
        w.schedule(k, 1_000, 0);
        let mut sessions = HashMap::new();
        assert_eq!(w.tick("t", 1_000, &mut sessions), 0);
        assert!(
            w.slots.iter().all(std::vec::Vec::is_empty),
            "stale key dropped"
        );
    }

    #[tokio::test]
    async fn tick_evicts_idle_session_and_releases_its_guards() {
        let mut w = IdleWheel::new();
        let client = addr("10.0.0.1:1");
        let k: SessionKey = (client, None);
        let tracker = ConnTracker::new();
        let s = session_tracked(&tracker, client, 0, 1_000).await;
        let mut sessions = HashMap::new();
        sessions.insert(k, s);
        w.schedule(k, 1_000, 0);
        assert_eq!(tracker.active(), 1);
        assert_eq!(w.tick("t", 999, &mut sessions), 0, "not idle yet: re-filed");
        assert_eq!(tracker.active(), 1);
        assert_eq!(w.tick("t", 1_000, &mut sessions), 1);
        assert!(sessions.is_empty());
        assert_eq!(tracker.active(), 0, "drain guard released on eviction");
        assert!(w.slots.iter().all(std::vec::Vec::is_empty));
    }

    #[tokio::test]
    async fn tick_refiles_a_refreshed_session_instead_of_evicting() {
        let mut w = IdleWheel::new();
        let client = addr("10.0.0.1:1");
        let k: SessionKey = (client, None);
        let s = session(client, 0, 2_000).await;
        let last = s.last_ms.clone();
        let mut sessions = HashMap::new();
        sessions.insert(k, s);
        w.schedule(k, 2_000, 0);

        // A datagram at t=1.5s pushes the deadline to 3.5s.
        last.store(1_500, Ordering::Relaxed);
        assert_eq!(w.tick("t", 1_000, &mut sessions), 0); // hand -> 1
        assert_eq!(w.tick("t", 2_000, &mut sessions), 0); // hand -> 2: due, refiled
        assert!(sessions.contains_key(&k));
        assert_eq!(slot_of(&w, &k), Some(3), "re-filed at its new deadline");

        assert_eq!(w.tick("t", 3_500, &mut sessions), 1); // hand -> 3: evicted
        assert!(sessions.is_empty());
    }

    #[tokio::test]
    async fn tick_rechecks_sessions_whose_idle_outruns_the_wheel() {
        let mut w = IdleWheel::new();
        let client = addr("10.0.0.1:1");
        let k: SessionKey = (client, None);
        let span_ms = WHEEL_SLOTS as u64 * 1_000;
        let s = session(client, 0, span_ms * 2).await;
        let mut sessions = HashMap::new();
        sessions.insert(k, s);
        w.schedule(k, span_ms * 2, 0);

        let mut now = 0;
        let mut evicted = 0;
        // One full revolution: the entry comes due early and must be re-filed.
        for _ in 0..WHEEL_SLOTS {
            now += 1_000;
            evicted += w.tick("t", now, &mut sessions);
        }
        assert_eq!(evicted, 0);
        assert!(sessions.contains_key(&k));
        assert_eq!(
            w.slots.iter().map(Vec::len).sum::<usize>(),
            1,
            "exactly one entry"
        );

        // Keep ticking until the real deadline: evicted exactly then.
        while sessions.contains_key(&k) {
            now += 1_000;
            evicted += w.tick("t", now, &mut sessions);
            assert!(now <= span_ms * 2 + 1_000, "evicted late at {now}");
        }
        assert_eq!(evicted, 1);
        assert!(now >= span_ms * 2, "evicted early at {now}");
    }
}
