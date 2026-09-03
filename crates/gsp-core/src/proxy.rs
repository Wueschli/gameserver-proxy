//! The per-connection byte pump.
//!
//! v0 uses a buffered user-space copy in both directions with a per-direction
//! idle timeout. The Linux `splice()` zero-copy fast path is a later
//! optimization and will slot in behind this same function.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::metrics_defs as m;
use crate::pool::Pool;

/// Connect / idle timeouts for a resolver `target` (no pool to read them from).
pub const TARGET_CONNECT_TIMEOUT: Duration = Duration::from_millis(300);
pub const TARGET_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// What a finished connection carries back to the caller for logging/metrics.
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
    pool: &Pool,
) -> anyhow::Result<ConnOutcome> {
    let guard = pool.acquire_for(Some(client_addr))?;
    let backend_addr = guard.addr();

    let mut backend =
        match connect_backend(backend_addr, pool.connect_timeout, transparent_source).await {
            Ok(s) => {
                guard.observe(true);
                s
            }
            Err(e) => {
                guard.observe(false);
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
            return Err(anyhow::anyhow!(
                "write PROXY header to backend {backend_addr} failed: {e}"
            ));
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
    proxy_protocol: gsp_config::ProxyProtocol,
) -> anyhow::Result<ConnOutcome> {
    let mut backend = connect_backend(target, connect_timeout, transparent_source).await?;

    let hdr = match proxy_protocol {
        gsp_config::ProxyProtocol::V1 | gsp_config::ProxyProtocol::V2 => {
            crate::proxy_protocol::header(proxy_protocol, client_addr, client_local)
        }
        _ => Vec::new(),
    };
    if !hdr.is_empty() {
        if let Err(e) = backend.write_all(&hdr).await {
            return Err(anyhow::anyhow!(
                "write PROXY header to target {target} failed: {e}"
            ));
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
) -> anyhow::Result<TcpStream> {
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
            Err(anyhow::anyhow!("connect to backend {addr} failed: {e}"))
        }
        Err(_) => {
            metrics::counter!(
                m::BACKEND_CONNECT_ERRORS, "backend" => addr.to_string(), "kind" => "timeout",
            )
            .increment(1);
            Err(anyhow::anyhow!("connect to backend {addr} timed out"))
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

    let (mut client_rd, mut client_wr) = client.into_split();
    let (mut backend_rd, mut backend_wr) = backend.into_split();

    let c2s = async {
        let n = copy_with_idle(&mut client_rd, &mut backend_wr, idle).await;
        let _ = backend_wr.shutdown().await;
        n
    };
    let s2c = async {
        let n = copy_with_idle(&mut backend_rd, &mut client_wr, idle).await;
        let _ = client_wr.shutdown().await;
        n
    };

    let (c2s_res, s2c_res) = tokio::join!(c2s, s2c);

    ConnOutcome {
        bytes_c2s: c2s_res.unwrap_or(0),
        bytes_s2c: s2c_res.unwrap_or(0),
        backend: backend_addr,
    }
}

/// Copy from `r` to `w` until EOF. Returns the number of bytes copied, or an
/// error if a read/write fails or no data arrives within `idle`.
///
/// Note (v0 simplification): the idle timer is per direction. A connection that
/// legitimately goes silent one way for longer than `idle` while the other way
/// is active will have its quiet half torn down. Game traffic is bidirectional
/// and frequent, so the default (90s) is comfortably safe; revisit when adding
/// the shared min-progress watchdog.
async fn copy_with_idle<R, W>(r: &mut R, w: &mut W, idle: Duration) -> std::io::Result<u64>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; 32 * 1024];
    let mut total: u64 = 0;
    loop {
        let n = match timeout(idle, r.read(&mut buf)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => n,
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "idle timeout",
                ))
            }
        };
        w.write_all(&buf[..n]).await?;
        total += n as u64;
    }
    Ok(total)
}
