//! The per-connection byte pump.
//!
//! Each direction is copied independently until its source reaches EOF, then the
//! half-close is propagated (`SHUT_WR`) so the peer sees it. An error or idle
//! expiry in either direction tears the whole connection down. On Linux the bytes
//! move through a kernel pipe with `splice(2)` — no userspace copy, the buffer
//! pair lives in the kernel; other platforms (or a pipe-setup failure) fall back
//! to a buffered `try_read` / `try_write` loop. The idle timer is shared: a
//! connection is idle only when *neither* direction has moved bytes for `idle`.
//! A direction stuck on a write (peer not reading) is therefore no longer bounded
//! by its own timer while the other direction keeps moving bytes.

use std::io;
use std::net::SocketAddr;
use std::pin::pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

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
/// side closes or the connection goes idle (no bytes either way) past
/// `pool.idle_timeout`.
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

    let progress = Progress::new(idle);
    let c2s_bytes = AtomicU64::new(0);
    let s2c_bytes = AtomicU64::new(0);

    {
        // `TcpStream`'s readiness/`try_*` API takes `&self`, so both directions
        // can share `&client` / `&backend` in one task — no `into_split`.
        let mut c2s = pin!(copy_one_way(&client, &backend, &c2s_bytes, &progress));
        let mut s2c = pin!(copy_one_way(&backend, &client, &s2c_bytes, &progress));
        let (mut c2s_done, mut s2c_done) = (false, false);
        // A clean EOF only finishes its own direction (the other may still be
        // streaming); an error or idle expiry in either one ends the connection,
        // dropping the other direction's future.
        while !(c2s_done && s2c_done) {
            let res = tokio::select! {
                r = &mut c2s, if !c2s_done => { c2s_done = true; r }
                r = &mut s2c, if !s2c_done => { s2c_done = true; r }
            };
            if res.is_err() {
                break;
            }
        }
    }
    // Both sockets drop here, so an aborted connection closes on both sides.

    ConnOutcome {
        bytes_c2s: c2s_bytes.load(Ordering::Relaxed),
        bytes_s2c: s2c_bytes.load(Ordering::Relaxed),
        backend: backend_addr,
    }
}

/// Shared idle watchdog for both directions of one connection: the clock is
/// reset whenever either direction moves bytes.
struct Progress {
    start: Instant,
    last_ms: AtomicU64,
    idle: Duration,
}

impl Progress {
    fn new(idle: Duration) -> Self {
        Self {
            start: Instant::now(),
            last_ms: AtomicU64::new(0),
            idle,
        }
    }

    fn touch(&self) {
        let now = u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.last_ms.fetch_max(now, Ordering::Relaxed);
    }

    /// Time left before the connection counts as idle; zero once it has.
    fn remaining(&self) -> Duration {
        let last = Duration::from_millis(self.last_ms.load(Ordering::Relaxed));
        (last + self.idle).saturating_sub(self.start.elapsed())
    }
}

/// Copy every byte from `from` to `to` until `from` reaches EOF, then propagate
/// the half-close so the peer sees it. `moved` accumulates the bytes delivered
/// to `to` as they go, so the total survives an error. Returns an error if a
/// read/write fails or the connection is idle longer than the shared watchdog.
async fn copy_one_way(
    from: &TcpStream,
    to: &TcpStream,
    moved: &AtomicU64,
    progress: &Progress,
) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    let res = match splice_impl::copy_spliced(from, to, moved, progress).await {
        Err(splice_impl::SpliceError::Setup) => copy_buffered(from, to, moved, progress).await,
        Err(splice_impl::SpliceError::Io(e)) => Err(e),
        Ok(()) => Ok(()),
    };
    #[cfg(not(target_os = "linux"))]
    let res = copy_buffered(from, to, moved, progress).await;

    // Best-effort `SHUT_WR` so the destination peer sees the EOF we just saw.
    // Ignored if the socket is already closed / errored.
    let _ = socket2::SockRef::from(to).shutdown(std::net::Shutdown::Write);
    res
}

/// Buffered fallback: a userspace `try_read` → `try_write` loop over the shared
/// `&TcpStream` refs. Used on non-Linux and if `splice`'s pipe setup fails.
async fn copy_buffered(
    from: &TcpStream,
    to: &TcpStream,
    moved: &AtomicU64,
    progress: &Progress,
) -> io::Result<()> {
    let mut buf = vec![0u8; 32 * 1024];
    loop {
        let n = loop {
            wait_ready(from, Interest::READABLE, progress).await?;
            match from.try_read(&mut buf) {
                Ok(n) => break n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Err(e) => return Err(e),
            }
        };
        if n == 0 {
            return Ok(());
        }
        progress.touch();
        let mut w = 0;
        while w < n {
            wait_ready(to, Interest::WRITABLE, progress).await?;
            match to.try_write(&buf[w..n]) {
                Ok(m) => {
                    w += m;
                    moved.fetch_add(m as u64, Ordering::Relaxed);
                    progress.touch();
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Err(e) => return Err(e),
            }
        }
    }
}

/// Await `interest` readiness on `sock`, mapping the connection going idle (no
/// progress in either direction for the shared window) to a `TimedOut` error.
async fn wait_ready(sock: &TcpStream, interest: Interest, progress: &Progress) -> io::Result<()> {
    loop {
        let left = progress.remaining();
        if left.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "idle timeout"));
        }
        // The other direction may move bytes while this one waits, so a timeout
        // re-checks the shared clock instead of failing outright.
        if let Ok(r) = timeout(left, sock.ready(interest)).await {
            return r.map(|_| ());
        }
    }
}

#[cfg(target_os = "linux")]
mod splice_impl {
    //! `splice(2)` fast path: bytes move `socket → pipe → socket` entirely in
    //! the kernel. One pipe pair per direction per connection, driven by tokio
    //! readiness via [`TcpStream::try_io`].

    use std::io;
    use std::os::fd::OwnedFd;
    use std::sync::atomic::{AtomicU64, Ordering};

    use nix::fcntl::{splice, OFlag, SpliceFFlags};
    use tokio::io::Interest;
    use tokio::net::TcpStream;

    use super::{wait_ready, Progress};

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
        moved: &AtomicU64,
        progress: &Progress,
    ) -> Result<(), SpliceError> {
        let (pipe_r, pipe_w) =
            nix::unistd::pipe2(OFlag::O_NONBLOCK).map_err(|_| SpliceError::Setup)?;
        loop {
            let n = socket_to_pipe(from, &pipe_w, progress)
                .await
                .map_err(SpliceError::Io)?;
            if n == 0 {
                return Ok(()); // source EOF
            }
            progress.touch();
            let mut left = n;
            while left > 0 {
                let n = pipe_to_socket(&pipe_r, to, left, progress)
                    .await
                    .map_err(SpliceError::Io)?;
                left -= n;
                moved.fetch_add(n as u64, Ordering::Relaxed);
                progress.touch();
            }
        }
    }

    /// `splice` from the source socket into the pipe, waiting on the socket's
    /// read readiness and retrying on `EAGAIN`. `Ok(0)` == socket EOF.
    async fn socket_to_pipe(
        sock: &TcpStream,
        pipe_w: &OwnedFd,
        progress: &Progress,
    ) -> io::Result<usize> {
        loop {
            wait_ready(sock, Interest::READABLE, progress).await?;
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
        progress: &Progress,
    ) -> io::Result<usize> {
        loop {
            wait_ready(sock, Interest::WRITABLE, progress).await?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn pair() -> (TcpStream, TcpStream) {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = TcpStream::connect(l.local_addr().unwrap()).await.unwrap();
        let (b, _) = l.accept().await.unwrap();
        (a, b)
    }

    /// Wire up `pump` between two loopback pairs: returns the far ends
    /// `(client_peer, backend_peer)` and the running pump task.
    async fn spawn_pump(
        idle: Duration,
    ) -> (TcpStream, TcpStream, tokio::task::JoinHandle<ConnOutcome>) {
        let (client_peer, client_side) = pair().await;
        let (backend_side, backend_peer) = pair().await;
        let addr = backend_peer.local_addr().unwrap();
        let h = tokio::spawn(pump(client_side, backend_side, addr, idle));
        (client_peer, backend_peer, h)
    }

    #[tokio::test]
    async fn bytes_are_counted_when_the_connection_ends_by_idle_timeout() {
        let (mut client, mut backend, h) = spawn_pump(Duration::from_millis(200)).await;
        client.write_all(&[1u8; 1000]).await.unwrap();
        backend.write_all(&[2u8; 300]).await.unwrap();

        let out = tokio::time::timeout(Duration::from_secs(5), h)
            .await
            .expect("idle timer should end the connection")
            .unwrap();
        assert_eq!(out.bytes_c2s, 1000);
        assert_eq!(out.bytes_s2c, 300);
    }

    #[tokio::test]
    async fn bytes_are_counted_when_the_backend_resets() {
        let (mut client, backend, h) = spawn_pump(Duration::from_secs(30)).await;
        client.write_all(&[1u8; 500]).await.unwrap();
        // Let the bytes land at the backend before it resets.
        tokio::time::sleep(Duration::from_millis(100)).await;
        socket2::SockRef::from(&backend)
            .set_linger(Some(Duration::ZERO))
            .unwrap();
        drop(backend);

        let out = tokio::time::timeout(Duration::from_secs(5), h)
            .await
            .expect("a reset must end the connection, not wait for idle")
            .unwrap();
        assert_eq!(out.bytes_c2s, 500);
    }

    #[tokio::test]
    async fn one_way_traffic_keeps_the_quiet_direction_open() {
        let idle = Duration::from_millis(300);
        let (mut client, mut backend, h) = spawn_pump(idle).await;

        // Backend streams for ~3x the idle window; the client never writes.
        for _ in 0..9 {
            backend.write_all(&[7u8; 100]).await.unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(!h.is_finished(), "an active connection must not idle out");

        // The quiet client direction must not have been half-closed: the
        // backend would see EOF here.
        let mut probe = [0u8; 1];
        assert!(
            tokio::time::timeout(Duration::from_millis(50), backend.read(&mut probe))
                .await
                .is_err(),
            "backend saw EOF/data although the client never closed"
        );
        let mut got = vec![0u8; 900];
        client.read_exact(&mut got).await.unwrap();

        // Once everything is quiet, the connection does idle out.
        let out = tokio::time::timeout(Duration::from_secs(5), h)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(out.bytes_s2c, 900);
        assert_eq!(out.bytes_c2s, 0);
    }

    #[tokio::test]
    async fn a_reset_in_one_direction_tears_down_the_other_promptly() {
        let (mut client, backend, h) = spawn_pump(Duration::from_secs(30)).await;
        socket2::SockRef::from(&backend)
            .set_linger(Some(Duration::ZERO))
            .unwrap();
        drop(backend);

        let out = tokio::time::timeout(Duration::from_secs(5), h).await;
        assert!(
            out.is_ok(),
            "pump should end on reset, not at the idle limit"
        );
        // The client side is closed too, not left dangling.
        let mut buf = [0u8; 1];
        let r = tokio::time::timeout(Duration::from_secs(2), client.read(&mut buf))
            .await
            .expect("client should observe the teardown");
        assert!(matches!(r, Ok(0) | Err(_)));
    }

    #[tokio::test]
    async fn a_clean_half_close_still_lets_the_other_direction_finish() {
        let (mut client, mut backend, h) = spawn_pump(Duration::from_secs(30)).await;
        client.write_all(b"req").await.unwrap();
        client.shutdown().await.unwrap();

        let mut req = Vec::new();
        backend.read_to_end(&mut req).await.unwrap(); // sees EOF after "req"
        assert_eq!(req, b"req");

        backend.write_all(b"response").await.unwrap();
        drop(backend);
        let mut resp = Vec::new();
        client.read_to_end(&mut resp).await.unwrap();
        assert_eq!(resp, b"response");

        let out = tokio::time::timeout(Duration::from_secs(5), h)
            .await
            .unwrap()
            .unwrap();
        assert_eq!((out.bytes_c2s, out.bytes_s2c), (3, 8));
    }
}
