//! The per-connection byte pump.
//!
//! Each direction is copied independently until its source reaches EOF, then the
//! half-close is propagated (`SHUT_WR`) so the peer sees it. On Linux the bytes
//! move through a kernel pipe with `splice(2)` — no userspace copy, the buffer
//! pair lives in the kernel; other platforms (or a pipe-setup failure) fall back
//! to a buffered `try_read` / `try_write` loop. The idle timer is per direction
//! (see the note on [`copy_buffered`]).

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncWriteExt, Interest};
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::drain::ConnGuard;
use crate::error::ProxyError;
use crate::metrics_defs as m;
use crate::pool::Pool;

/// Default connect / idle timeouts for a resolver `target` (no pool to read them
/// from). Overridable per resolver via `resolvers[].target_connect_timeout_ms` /
/// `target_idle_timeout_sec`; these are the fallback for a resolver that does not
/// set them and for push-hint / direct-config targets.
pub const TARGET_CONNECT_TIMEOUT: Duration = Duration::from_millis(300);
pub const TARGET_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// What a finished connection carries back to the caller for logging/metrics.
#[derive(Debug)]
pub struct ConnOutcome {
    pub bytes_c2s: u64,
    pub bytes_s2c: u64,
    pub backend: SocketAddr,
}

/// Select a backend from `pool`, connect, and pump bytes both ways until either
/// side closes or a direction goes idle past `pool.idle_timeout`.
///
/// The [`BackendGuard`](crate::pool::BackendGuard) returned by `acquire` holds
/// an active-session slot for the whole connection (released on drop) and
/// carries passive connect results back into the backend's health state.
pub async fn handle_tcp(
    client: TcpStream,
    client_addr: SocketAddr,
    client_local: SocketAddr,
    transparent_source: Option<SocketAddr>,
    conn: &ConnGuard,
    pool: &Pool,
) -> Result<ConnOutcome, ProxyError> {
    let guard = pool.acquire_for(Some(client_addr))?;
    let backend_addr = guard.addr();
    conn.set_target(Some(pool.name.as_ref()), backend_addr);

    let mut backend =
        match connect_backend(backend_addr, pool.connect_timeout, transparent_source).await {
            Ok(s) => {
                guard.observe(true);
                s
            }
            Err(e) => {
                // Running out of fds or ports is ours, not the backend's.
                let local = matches!(&e, ProxyError::Connect { source, .. }
                    if crate::util::is_local_resource_error(source));
                if !local {
                    guard.observe(false);
                }
                return Err(e);
            }
        };

    // PROXY protocol header (if the pool asks for one) goes out before any
    // client bytes so the backend can parse it as the first thing on the wire.
    // v1/v2 only on TCP; v2-udp is a UDP-listener form and never applies here
    // (rejected at config validation for a static route; a resolver-chosen pool
    // with the wrong form just sends no header).
    let pp = pool.proxy_protocol;
    let hdr = match pp {
        gsp_config::ProxyProtocol::V1 | gsp_config::ProxyProtocol::V2 => {
            crate::proxy_protocol::header(pp, client_addr, client_local)
        }
        _ => Vec::new(),
    };
    if !hdr.is_empty() {
        if let Err(e) = backend.write_all(&hdr).await {
            guard.observe(false);
            return Err(ProxyError::ProxyHeader {
                role: "backend",
                addr: backend_addr,
                source: e,
            });
        }
        metrics::counter!(
            m::PROXY_PROTOCOL_HEADERS, "pool" => pool.name.to_string(), "version" => pp.label(),
        )
        .increment(1);
    }

    Ok(pump(client, backend, backend_addr, pool.idle_timeout).await)
}

/// Like [`handle_tcp`], but to a resolver-supplied fixed instance — no pool, so
/// no health check, no per-backend cap, no [`BackendGuard`].
///
/// `proxy_protocol` comes from the choosing resolver's `proxy_protocol:` (there
/// is no pool to read it from); a v1/v2 header is written before any client
/// bytes, exactly as for a pooled connection. `v2-udp` / `none` write nothing.
#[allow(clippy::too_many_arguments)]
pub async fn handle_tcp_target(
    client: TcpStream,
    client_addr: SocketAddr,
    client_local: SocketAddr,
    target: SocketAddr,
    connect_timeout: Duration,
    idle_timeout: Duration,
    transparent_source: Option<SocketAddr>,
    conn: &ConnGuard,
    proxy_protocol: gsp_config::ProxyProtocol,
) -> Result<ConnOutcome, ProxyError> {
    conn.set_target(Some("(resolver target)"), target);
    let mut backend = connect_backend(target, connect_timeout, transparent_source).await?;

    let hdr = match proxy_protocol {
        gsp_config::ProxyProtocol::V1 | gsp_config::ProxyProtocol::V2 => {
            crate::proxy_protocol::header(proxy_protocol, client_addr, client_local)
        }
        _ => Vec::new(),
    };
    if !hdr.is_empty() {
        if let Err(e) = backend.write_all(&hdr).await {
            return Err(ProxyError::ProxyHeader {
                role: "target",
                addr: target,
                source: e,
            });
        }
        metrics::counter!(
            m::PROXY_PROTOCOL_HEADERS,
            "pool" => "(resolver target)",
            "version" => proxy_protocol.label(),
        )
        .increment(1);
    }

    Ok(pump(client, backend, target, idle_timeout).await)
}

async fn connect_backend(
    addr: SocketAddr,
    connect_timeout: Duration,
    transparent_source: Option<SocketAddr>,
) -> Result<TcpStream, ProxyError> {
    match timeout(
        connect_timeout,
        crate::net::connect_tcp_from(addr, transparent_source),
    )
    .await
    {
        Ok(Ok(s)) => Ok(s),
        Ok(Err(e)) => {
            metrics::counter!(
                m::BACKEND_CONNECT_ERRORS, "backend" => addr.to_string(), "kind" => "refused",
            )
            .increment(1);
            Err(ProxyError::Connect { addr, source: e })
        }
        Err(_) => {
            metrics::counter!(
                m::BACKEND_CONNECT_ERRORS, "backend" => addr.to_string(), "kind" => "timeout",
            )
            .increment(1);
            Err(ProxyError::ConnectTimeout { addr })
        }
    }
}

async fn pump(
    client: TcpStream,
    backend: TcpStream,
    backend_addr: SocketAddr,
    idle: Duration,
) -> ConnOutcome {
    let _ = client.set_nodelay(true);
    let _ = backend.set_nodelay(true);

    // `TcpStream`'s readiness/`try_*` API takes `&self`, so both directions can
    // share `&client` / `&backend` in one task — no `into_split`.
    let c2s = copy_one_way(&client, &backend, idle);
    let s2c = copy_one_way(&backend, &client, idle);
    let (c2s_res, s2c_res) = tokio::join!(c2s, s2c);

    ConnOutcome {
        bytes_c2s: c2s_res.unwrap_or(0),
        bytes_s2c: s2c_res.unwrap_or(0),
        backend: backend_addr,
    }
}

/// Copy every byte from `from` to `to` until `from` reaches EOF, then propagate
/// the half-close so the peer sees it. Returns the byte count, or an error if a
/// read/write fails or `from` is idle longer than `idle`.
async fn copy_one_way(from: &TcpStream, to: &TcpStream, idle: Duration) -> io::Result<u64> {
    #[cfg(target_os = "linux")]
    let res = match splice_impl::copy_spliced(from, to, idle).await {
        Err(splice_impl::SpliceError::Setup) => copy_buffered(from, to, idle).await,
        Err(splice_impl::SpliceError::Io(e)) => Err(e),
        Ok(n) => Ok(n),
    };
    #[cfg(not(target_os = "linux"))]
    let res = copy_buffered(from, to, idle).await;

    // Best-effort `SHUT_WR` so the destination peer sees the EOF we just saw.
    // Ignored if the socket is already closed / errored.
    let _ = socket2::SockRef::from(to).shutdown(std::net::Shutdown::Write);
    res
}

/// Buffered fallback: a userspace `try_read` → `try_write` loop over the shared
/// `&TcpStream` refs. Used on non-Linux and if `splice`'s pipe setup fails.
///
/// Note (v0 simplification): the idle timer is per direction. A connection that
/// legitimately goes silent one way for longer than `idle` while the other way
/// is active will have its quiet half torn down. Game traffic is bidirectional
/// and frequent, so the default (90s) is comfortably safe; revisit when adding
/// the shared min-progress watchdog.
async fn copy_buffered(from: &TcpStream, to: &TcpStream, idle: Duration) -> io::Result<u64> {
    let mut buf = vec![0u8; 32 * 1024];
    let mut total: u64 = 0;
    loop {
        let n = loop {
            wait_ready(from, Interest::READABLE, idle).await?;
            match from.try_read(&mut buf) {
                Ok(n) => break n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Err(e) => return Err(e),
            }
        };
        if n == 0 {
            break;
        }
        let mut w = 0;
        while w < n {
            wait_ready(to, Interest::WRITABLE, idle).await?;
            match to.try_write(&buf[w..n]) {
                Ok(m) => w += m,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Err(e) => return Err(e),
            }
        }
        total += n as u64;
    }
    Ok(total)
}

/// Await `interest` readiness on `sock`, mapping the idle deadline to a
/// `TimedOut` error.
async fn wait_ready(sock: &TcpStream, interest: Interest, idle: Duration) -> io::Result<()> {
    match timeout(idle, sock.ready(interest)).await {
        Ok(r) => r.map(|_| ()),
        Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, "idle timeout")),
    }
}

#[cfg(target_os = "linux")]
mod splice_impl {
    //! `splice(2)` fast path: bytes move `socket → pipe → socket` entirely in
    //! the kernel. One pipe pair per direction per connection, driven by tokio
    //! readiness via [`TcpStream::try_io`].

    use std::io;
    use std::os::fd::OwnedFd;
    use std::time::Duration;

    use nix::fcntl::{splice, OFlag, SpliceFFlags};
    use tokio::io::Interest;
    use tokio::net::TcpStream;

    use super::wait_ready;

    /// Default pipe capacity is 64 KiB; move one that big per syscall.
    const CHUNK: usize = 64 * 1024;
    const FLAGS: SpliceFFlags = SpliceFFlags::SPLICE_F_MOVE.union(SpliceFFlags::SPLICE_F_NONBLOCK);

    pub enum SpliceError {
        /// Pipe creation failed before any byte moved — the caller falls back to
        /// the buffered copy.
        Setup,
        Io(io::Error),
    }

    pub async fn copy_spliced(
        from: &TcpStream,
        to: &TcpStream,
        idle: Duration,
    ) -> Result<u64, SpliceError> {
        let (pipe_r, pipe_w) =
            nix::unistd::pipe2(OFlag::O_NONBLOCK).map_err(|_| SpliceError::Setup)?;
        let mut total: u64 = 0;
        loop {
            let moved = socket_to_pipe(from, &pipe_w, idle)
                .await
                .map_err(SpliceError::Io)?;
            if moved == 0 {
                return Ok(total); // source EOF
            }
            let mut left = moved;
            while left > 0 {
                let n = pipe_to_socket(&pipe_r, to, left, idle)
                    .await
                    .map_err(SpliceError::Io)?;
                left -= n;
                total += n as u64;
            }
        }
    }

    /// `splice` from the source socket into the pipe, waiting on the socket's
    /// read readiness and retrying on `EAGAIN`. `Ok(0)` == socket EOF.
    async fn socket_to_pipe(
        sock: &TcpStream,
        pipe_w: &OwnedFd,
        idle: Duration,
    ) -> io::Result<usize> {
        loop {
            wait_ready(sock, Interest::READABLE, idle).await?;
            let out = sock.try_io(Interest::READABLE, || {
                splice(sock, None, pipe_w, None, CHUNK, FLAGS)
                    .map_err(|e| io::Error::from_raw_os_error(e as i32))
            });
            match out {
                Ok(n) => return Ok(n),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Err(e) => return Err(e),
            }
        }
    }

    /// `splice` `len` bytes from the pipe into the destination socket, waiting on
    /// the socket's write readiness and retrying on `EAGAIN`.
    async fn pipe_to_socket(
        pipe_r: &OwnedFd,
        sock: &TcpStream,
        len: usize,
        idle: Duration,
    ) -> io::Result<usize> {
        loop {
            wait_ready(sock, Interest::WRITABLE, idle).await?;
            let out = sock.try_io(Interest::WRITABLE, || {
                match splice(pipe_r, None, sock, None, len, FLAGS)
                    .map_err(|e| io::Error::from_raw_os_error(e as i32))?
                {
                    0 => Err(io::Error::new(io::ErrorKind::WriteZero, "splice wrote 0")),
                    n => Ok(n),
                }
            });
            match out {
                Ok(n) => return Ok(n),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Err(e) => return Err(e),
            }
        }
    }
}
