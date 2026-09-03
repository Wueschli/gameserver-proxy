//! Low-level socket helpers.

use std::net::SocketAddr;

use socket2::{Domain, Protocol, Socket, Type};

/// Bind a UDP socket with `SO_REUSEADDR` / `SO_REUSEPORT` so every worker gets
/// its own socket on the same port and the kernel pins each client 4-tuple to
/// one worker (keeping the per-worker session table lock-free).
pub fn bind_reuseport_udp(addr: SocketAddr) -> std::io::Result<std::net::UdpSocket> {
    let sock = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_reuse_address(true)?;
    #[cfg(unix)]
    sock.set_reuse_port(true)?;
    sock.set_nonblocking(true)?;
    sock.bind(&addr.into())?;
    Ok(sock.into())
}

/// Bind a TCP listening socket with `SO_REUSEADDR` and (on Unix) `SO_REUSEPORT`
/// so that multiple worker tasks — and, later, multiple processes — can share
/// the same port with the kernel spreading accepts across them.
pub fn bind_reuseport_tcp(
    addr: SocketAddr,
    backlog: i32,
) -> std::io::Result<std::net::TcpListener> {
    let sock = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    sock.set_reuse_address(true)?;
    #[cfg(unix)]
    sock.set_reuse_port(true)?;
    sock.set_nonblocking(true)?;
    sock.bind(&addr.into())?;
    sock.listen(backlog)?;
    Ok(sock.into())
}
