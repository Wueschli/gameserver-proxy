//! Low-level socket helpers.

use std::net::SocketAddr;
use std::os::fd::AsFd;
use std::sync::atomic::{AtomicU64, Ordering};

use socket2::{Domain, Protocol, SockRef, Socket, Type};
use tokio::net::{TcpSocket, TcpStream};

/// How a UDP listener learns the real destination address of each datagram.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UdpMode {
    /// Plain `recv_from`; the destination is the socket's own bind address.
    Plain,
    /// Prefix mode: `IP_PKTINFO` / `IPV6_RECVPKTINFO` — one wildcard socket
    /// serves a routed prefix, the destination IP is read per datagram.
    Prefix,
    /// Transparent mode (Linux TPROXY): `IP_TRANSPARENT` + `IP_RECVORIGDSTADDR`
    /// — the socket receives datagrams redirected to non-local addresses and the
    /// original destination `ip:port` is read per datagram.
    Transparent,
}

/// Bind a UDP socket with `SO_REUSEADDR` / `SO_REUSEPORT` so every worker gets
/// its own socket on the same port and the kernel pins each client 4-tuple to
/// one worker (keeping the per-worker session table lock-free).
///
/// `mode` selects the per-datagram destination mechanism (see [`UdpMode`]).
pub fn bind_reuseport_udp(addr: SocketAddr, mode: UdpMode) -> std::io::Result<std::net::UdpSocket> {
    let sock = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_reuse_address(true)?;
    #[cfg(unix)]
    sock.set_reuse_port(true)?;
    sock.set_nonblocking(true)?;
    match mode {
        UdpMode::Plain => {}
        UdpMode::Prefix => {
            use nix::sys::socket::{setsockopt, sockopt};
            if addr.is_ipv6() {
                setsockopt(&sock, sockopt::Ipv6RecvPacketInfo, &true)
                    .map_err(std::io::Error::from)?;
            } else {
                setsockopt(&sock, sockopt::Ipv4PacketInfo, &true).map_err(std::io::Error::from)?;
            }
        }
        UdpMode::Transparent => {
            use nix::sys::socket::{setsockopt, sockopt};
            set_ip_transparent(&sock, addr.is_ipv6())?;
            if addr.is_ipv6() {
                setsockopt(&sock, sockopt::Ipv6OrigDstAddr, &true).map_err(std::io::Error::from)?;
            } else {
                setsockopt(&sock, sockopt::Ipv4OrigDstAddr, &true).map_err(std::io::Error::from)?;
            }
        }
    }
    sock.bind(&addr.into())?;
    Ok(sock.into())
}

/// Bind a UDP socket with `IP_TRANSPARENT` to `addr` (a non-local address) so it
/// can be the *source* of datagrams sent from it — the reply path in UDP
/// transparent mode, where the client must see replies coming from the address
/// it originally addressed.
pub fn bind_transparent_udp(addr: SocketAddr) -> std::io::Result<std::net::UdpSocket> {
    let sock = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_reuse_address(true)?;
    #[cfg(unix)]
    sock.set_reuse_port(true)?;
    sock.set_nonblocking(true)?;
    set_ip_transparent(&sock, addr.is_ipv6())?;
    sock.bind(&addr.into())?;
    Ok(sock.into())
}

/// Bind a TCP listening socket with `SO_REUSEADDR` and (on Unix) `SO_REUSEPORT`
/// so that multiple worker tasks — and, later, multiple processes — can share
/// the same port with the kernel spreading accepts across them.
///
/// With `freebind`, set `IP_FREEBIND` / `IPV6_FREEBIND` so the socket can bind
/// an address that is not (yet) configured on a local interface.
/// With `transparent`, also set `IP_TRANSPARENT` (Linux) so the socket accepts
/// connections that were TPROXY-redirected to a non-local address; `getsockname`
/// on each accepted stream then returns the original destination.
pub fn bind_reuseport_tcp(
    addr: SocketAddr,
    backlog: i32,
    freebind: bool,
    transparent: bool,
) -> std::io::Result<std::net::TcpListener> {
    let sock = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    sock.set_reuse_address(true)?;
    #[cfg(unix)]
    sock.set_reuse_port(true)?;
    sock.set_nonblocking(true)?;
    if freebind {
        if addr.is_ipv6() {
            sock.set_freebind_v6(true)?;
        } else {
            sock.set_freebind_v4(true)?;
        }
    }
    if transparent {
        set_ip_transparent(&sock, addr.is_ipv6())?;
    }
    sock.bind(&addr.into())?;
    sock.listen(backlog)?;
    Ok(sock.into())
}

/// Minimum gap between two family-mismatch warnings (the mismatch recurs on every
/// connection, so an unthrottled `warn` floods the log).
const FAMILY_WARN_EVERY_MS: u64 = 60_000;
/// `mono_ms` of the last family-mismatch warning; `u64::MAX` = none yet.
static LAST_FAMILY_WARN: AtomicU64 = AtomicU64::new(u64::MAX);

/// True at most once per `every_ms`: claims the slot in `last` with a CAS, so
/// concurrent callers (one per worker) log it once between them, lock-free.
fn warn_due(last: &AtomicU64, now_ms: u64, every_ms: u64) -> bool {
    let prev = last.load(Ordering::Relaxed);
    if prev != u64::MAX && now_ms.saturating_sub(prev) < every_ms {
        return false;
    }
    last.compare_exchange(prev, now_ms, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
}

/// Open a TCP connection to `backend`. With `source` set, bind that address as
/// the local end first, with `IP_TRANSPARENT` so the kernel keeps a non-local
/// address (the real client's) as the packet source — Linux TPROXY transparent
/// mode. `source` must be the same address family as `backend`; a mismatch
/// falls back to an ordinary connect.
pub async fn connect_tcp_from(
    backend: SocketAddr,
    source: Option<SocketAddr>,
) -> std::io::Result<TcpStream> {
    let Some(src) = source.filter(|s| s.is_ipv4() == backend.is_ipv4()) else {
        if source.is_some()
            && warn_due(
                &LAST_FAMILY_WARN,
                crate::util::mono_ms(),
                FAMILY_WARN_EVERY_MS,
            )
        {
            tracing::warn!(%backend, "transparent connect: client/backend family mismatch; \
                connecting without a bound source (logged at most once a minute)");
        }
        return TcpStream::connect(backend).await;
    };
    let sock = if backend.is_ipv4() {
        TcpSocket::new_v4()?
    } else {
        TcpSocket::new_v6()?
    };
    set_ip_transparent(&sock, backend.is_ipv6())?;
    sock.set_reuseaddr(true)?;
    sock.bind(src)?;
    sock.connect(backend).await
}

/// Set `IP_TRANSPARENT` (v4) or `IPV6_TRANSPARENT` (v6) on a socket. Linux-only;
/// an `Unsupported` error elsewhere.
pub fn set_ip_transparent<F: AsFd>(sock: &F, v6: bool) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let r = SockRef::from(sock);
        if v6 {
            r.set_ip_transparent_v6(true)
        } else {
            r.set_ip_transparent_v4(true)
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (sock, v6);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "IP_TRANSPARENT is Linux-only",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn connect_tcp_from_without_a_source_connects_normally() {
        let srv = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = srv.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = srv.accept().await.unwrap();
            let _ = s.write_all(b"ok").await;
        });
        let mut c = connect_tcp_from(addr, None).await.unwrap();
        let mut buf = [0u8; 2];
        c.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ok");
    }

    #[test]
    fn warn_due_fires_once_per_interval() {
        let last = AtomicU64::new(u64::MAX);
        assert!(warn_due(&last, 0, 100), "first call always warns");
        assert!(!warn_due(&last, 99, 100));
        assert!(warn_due(&last, 100, 100));
        assert!(!warn_due(&last, 150, 100));
    }

    #[tokio::test]
    async fn connect_tcp_from_falls_back_on_a_family_mismatch() {
        let srv = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = srv.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = srv.accept().await.unwrap();
            let _ = s.write_all(b"ok").await;
        });
        // IPv6 source, IPv4 backend: the source is ignored, connect still works.
        let src = Some("[::1]:0".parse().unwrap());
        let mut c = connect_tcp_from(addr, src).await.unwrap();
        let mut buf = [0u8; 2];
        c.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ok");
    }
}
