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
    pool: &Pool,
) -> anyhow::Result<ConnOutcome> {
    let guard = pool.acquire_for(Some(client_addr))?;
    let backend_addr = guard.addr();

    let backend = match timeout(pool.connect_timeout, TcpStream::connect(backend_addr)).await {
        Ok(Ok(s)) => {
            guard.observe(true);
            s
        }
        Ok(Err(e)) => {
            guard.observe(false);
            metrics::counter!(
                m::BACKEND_CONNECT_ERRORS,
                "backend" => backend_addr.to_string(),
                "kind" => "refused",
            )
            .increment(1);
            return Err(anyhow::anyhow!(
                "connect to backend {backend_addr} failed: {e}"
            ));
        }
        Err(_) => {
            guard.observe(false);
            metrics::counter!(
                m::BACKEND_CONNECT_ERRORS,
                "backend" => backend_addr.to_string(),
                "kind" => "timeout",
            )
            .increment(1);
            return Err(anyhow::anyhow!(
                "connect to backend {backend_addr} timed out"
            ));
        }
    };

    let _ = client.set_nodelay(true);
    let _ = backend.set_nodelay(true);

    let (mut client_rd, mut client_wr) = client.into_split();
    let (mut backend_rd, mut backend_wr) = backend.into_split();

    let idle = pool.idle_timeout;

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

    Ok(ConnOutcome {
        bytes_c2s: c2s_res.unwrap_or(0),
        bytes_s2c: s2c_res.unwrap_or(0),
        backend: backend_addr,
    })
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
