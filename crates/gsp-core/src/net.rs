//! Low-level socket helpers.

use std::net::SocketAddr;

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::{TcpSocket, TcpStream};

/// Bind a UDP socket with `SO_REUSEADDR` / `SO_REUSEPORT` so every worker gets
/// its own socket on the same port and the kernel pins each client 4-tuple to
/// one worker (keeping the per-worker session table lock-free).
///
/// With `pktinfo`, also request `IP_PKTINFO` / `IPV6_RECVPKTINFO` so the recv
/// path can read the real destination address of each datagram (prefix mode).
pub fn bind_reuseport_udp(addr: SocketAddr, pktinfo: bool) -> std::io::Result<std::net::UdpSocket> {
    let sock = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_reuse_address(true)?;
    #[cfg(unix)]
    sock.set_reuse_port(true)?;
    sock.set_nonblocking(true)?;
    if pktinfo {
        use nix::sys::socket::setsockopt;
        use nix::sys::socket::sockopt;
        if addr.is_ipv6() {
            setsockopt(&sock, sockopt::Ipv6RecvPacketInfo, &true).map_err(std::io::Error::from)?;
        } else {
            setsockopt(&sock, sockopt::Ipv4PacketInfo, &true).map_err(std::io::Error::from)?;
        }
    }
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
            sock.set_freebind_ipv6(true)?;
        } else {
            sock.set_freebind(true)?;
        }
    }
    if transparent {
        set_ip_transparent(&sock)?;
    }
    sock.bind(&addr.into())?;
    sock.listen(backlog)?;
    Ok(sock.into())
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
        if source.is_some() {
            tracing::warn!(%backend, "transparent connect: client/backend family mismatch; \
                connecting without a bound source");
        }
        return TcpStream::connect(backend).await;
    };
    let sock = if backend.is_ipv4() {
        TcpSocket::new_v4()?
    } else {
        TcpSocket::new_v6()?
    };
    set_ip_transparent(&sock)?;
    sock.set_reuseaddr(true)?;
    sock.bind(src)?;
    sock.connect(backend).await
}

/// Set `IP_TRANSPARENT` on a socket. Linux-only; a no-op error elsewhere.
fn set_ip_transparent<F: std::os::fd::AsFd>(sock: &F) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use nix::sys::socket::{setsockopt, sockopt};
        setsockopt(sock, sockopt::IpTransparent, &true).map_err(std::io::Error::from)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = sock;
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
