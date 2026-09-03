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

/// Session table key: the client address, plus (in prefix mode) the destination
/// address the datagram was sent to.
type SessionKey = (SocketAddr, Option<IpAddr>);

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
enum Who {
    Ip(IpAddr),
    IpPort(SocketAddr),
}

#[derive(PartialEq, Eq, Hash)]
struct StickyKey {
    dst: Option<IpAddr>,
    who: Who,
}

fn sticky_key(
    affinity: Option<HashOn>,
    client: SocketAddr,
    dst: Option<IpAddr>,
) -> Option<StickyKey> {
    let who = match affinity? {
        HashOn::SrcIp => Who::Ip(client.ip()),
        HashOn::SrcIpPort => Who::IpPort(client),
    };
    Some(StickyKey { dst, who })
}

pub async fn run_udp_listener(
    cfg: ListenerConfig,
    snapshot: Arc<ArcSwap<Snapshot>>,
    worker_id: usize,
    shutdown: &mut watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let pktinfo = cfg.prefix.is_some();
    let sock = Arc::new(UdpSocket::from_std(bind_reuseport_udp(cfg.bind, pktinfo)?)?);
    tracing::info!(
        listener = %cfg.name,
        worker = worker_id,
        bind = %cfg.bind,
        prefix = ?cfg.prefix,
        "udp listener started"
    );

    let mut sessions: HashMap<SessionKey, Session> = HashMap::new();
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
            recv = recv_one(&sock, &mut buf, pktinfo) => {
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
                        Some(ip) if prefix.contains(ip) => {}
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

                // New session.
                match open_session(&cfg, &snapshot, &sock, &mut sticky, client, dst, &buf[..n]).await {
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

/// Receive one datagram. In plain mode this is `recv_from`; in prefix mode it is
/// `recvmsg` with an `IP_PKTINFO` / `IPV6_PKTINFO` control message, yielding the
/// real destination address.
async fn recv_one(
    sock: &UdpSocket,
    buf: &mut [u8],
    pktinfo: bool,
) -> io::Result<(usize, SocketAddr, Option<IpAddr>)> {
    if !pktinfo {
        let (n, from) = sock.recv_from(buf).await?;
        return Ok((n, from, None));
    }
    loop {
        sock.readable().await?;
        match sock.try_io(Interest::READABLE, || recvmsg_pktinfo(sock, &mut *buf)) {
            Ok(v) => return Ok(v),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(e),
        }
    }
}

fn recvmsg_pktinfo(
    sock: &UdpSocket,
    buf: &mut [u8],
) -> io::Result<(usize, SocketAddr, Option<IpAddr>)> {
    use nix::sys::socket::{recvmsg, ControlMessageOwned, MsgFlags, SockaddrStorage};

    let mut iov = [IoSliceMut::new(buf)];
    let mut cmsg = nix::cmsg_space!(nix::libc::in6_pktinfo);
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
            ControlMessageOwned::Ipv4PacketInfo(pi) => {
                dst = Some(IpAddr::V4(Ipv4Addr::from(pi.ipi_addr.s_addr.to_ne_bytes())));
            }
            ControlMessageOwned::Ipv6PacketInfo(pi) => {
                dst = Some(IpAddr::V6(Ipv6Addr::from(pi.ipi6_addr.s6_addr)));
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
    down: &Arc<UdpSocket>,
    sticky: &mut HashMap<StickyKey, SocketAddr>,
    client: SocketAddr,
    dst: Option<IpAddr>,
    first: &[u8],
) -> Result<Session, &'static str> {
    let snap = snapshot.load_full();
    let local = match dst {
        Some(ip) => SocketAddr::new(ip, cfg.bind.port()),
        None => down.local_addr().unwrap_or(cfg.bind),
    };
    let mctx = gsp_config::MatchContext {
        src: client,
        local,
        first_bytes: first,
    };
    let pool_name = cfg.route_for(&mctx).ok_or("no_route")?;
    let pool = snap.pool(pool_name).ok_or("no_route")?;
    let idle_ms = pool.idle_timeout.as_millis() as u64;

    let skey = sticky_key(cfg.affinity, client, dst);
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
        dst,
        last_ms.clone(),
    );

    tracing::debug!(listener = %cfg.name, %client, ?dst, %backend, "udp session opened");
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
/// session entry afterwards. `reply_src` is `Some` in prefix mode: the reply is
/// sent with that address as its source (`sendmsg` + pktinfo cmsg).
fn spawn_reply(
    listener: String,
    down: Arc<UdpSocket>,
    up: Arc<UdpSocket>,
    client: SocketAddr,
    reply_src: Option<IpAddr>,
    last_ms: Arc<AtomicU64>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut buf = vec![0u8; MAX_DATAGRAM];
        loop {
            match up.recv(&mut buf).await {
                Ok(n) => {
                    last_ms.store(now_ms(), Ordering::Relaxed);
                    if let Err(e) = send_reply(&down, &buf[..n], client, reply_src).await {
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
