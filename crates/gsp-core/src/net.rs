//! Low-level socket helpers.

use std::net::SocketAddr;

use socket2::{Domain, Protocol, Socket, Type};

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
pub fn bind_reuseport_tcp(
    addr: SocketAddr,
    backlog: i32,
    freebind: bool,
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
    sock.bind(&addr.into())?;
    sock.listen(backlog)?;
    Ok(sock.into())
}
