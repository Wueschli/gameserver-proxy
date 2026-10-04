//! A TCP + UDP echo server that runs *inside* a network namespace (on a
//! thread that has `setns`-ed into it) plus the matching round-trip clients.
//! Stands in for a game server in the tunnel e2e test. It binds `[::]`
//! (dual stack, so IPv4 and IPv6 tunnel addresses both reach it), so it works
//! before the origin's WireGuard interface (and its tunnel address) exists.

use std::net::SocketAddr;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::oneshot;
use tokio::time::timeout;

use crate::netns::Ns;

pub struct EchoServer {
    stop: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<Result<Result<()>>>>,
}

impl EchoServer {
    pub fn start(ns: &Ns, port: u16) -> Result<Self> {
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<std::result::Result<(), String>>();

        let thread = ns.spawn_thread(move || -> Result<()> {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            rt.block_on(async move {
                let bound = async {
                    let tcp = TcpListener::bind(("::", port)).await?;
                    let udp = UdpSocket::bind(("::", port)).await?;
                    std::io::Result::Ok((tcp, udp))
                }
                .await;
                let (tcp, udp) = match bound {
                    Ok(pair) => pair,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e.to_string()));
                        return Ok(());
                    }
                };
                let _ = ready_tx.send(Ok(()));

                let mut stop_rx = stop_rx;
                let mut buf = vec![0u8; 65536];
                loop {
                    tokio::select! {
                        _ = &mut stop_rx => break,
                        accepted = tcp.accept() => {
                            if let Ok((stream, _)) = accepted {
                                tokio::spawn(async move {
                                    let (mut r, mut w) = stream.into_split();
                                    let _ = tokio::io::copy(&mut r, &mut w).await;
                                });
                            }
                        }
                        got = udp.recv_from(&mut buf) => {
                            if let Ok((n, from)) = got {
                                let _ = udp.send_to(&buf[..n], from).await;
                            }
                        }
                    }
                }
                Ok(())
            })
        })?;

        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Self {
                stop: Some(stop_tx),
                thread: Some(thread),
            }),
            Ok(Err(e)) => bail!("echo server failed to bind port {port}: {e}"),
            // The thread ended without reporting: `setns` into the namespace
            // (or building the runtime) failed, and its error is the cause.
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => match thread.join() {
                Ok(Err(e)) | Ok(Ok(Err(e))) => {
                    Err(e).context(format!("echo server on port {port} could not start"))
                }
                Ok(Ok(Ok(()))) => bail!("echo server on port {port} exited before it was ready"),
                Err(_) => bail!("echo server thread on port {port} panicked before it was ready"),
            },
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                bail!("echo server did not report ready within 5s")
            }
        }
    }
}

impl Drop for EchoServer {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Connect, send `payload`, and read back exactly as many bytes. Sending and
/// receiving overlap, so payloads larger than the socket buffers don't deadlock.
pub async fn tcp_roundtrip(addr: SocketAddr, payload: &[u8]) -> Result<Vec<u8>> {
    timeout(Duration::from_secs(5), async {
        let stream = TcpStream::connect(addr).await.context("connect")?;
        let (mut r, mut w) = stream.into_split();
        let out = payload.to_vec();
        let writer = tokio::spawn(async move {
            w.write_all(&out).await?;
            std::io::Result::Ok(w) // keep the write half open until we've read everything
        });
        let mut got = vec![0u8; payload.len()];
        r.read_exact(&mut got).await.context("read echo")?;
        let _w = writer.await.map_err(|e| anyhow!("writer task: {e}"))??;
        Ok(got)
    })
    .await
    .map_err(|_| anyhow!("tcp round trip to {addr} timed out"))?
}

/// One datagram out, one datagram back.
pub async fn udp_roundtrip(addr: SocketAddr, payload: &[u8]) -> Result<Vec<u8>> {
    timeout(Duration::from_secs(5), async {
        let any = if addr.is_ipv6() {
            "[::]:0"
        } else {
            "0.0.0.0:0"
        };
        let sock = UdpSocket::bind(any).await?;
        sock.connect(addr).await?;
        sock.send(payload).await?;
        let mut buf = vec![0u8; 65536];
        let n = sock.recv(&mut buf).await?;
        buf.truncate(n);
        Ok(buf)
    })
    .await
    .map_err(|_| anyhow!("udp round trip to {addr} timed out"))?
}
