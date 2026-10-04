//! Shared helpers between the two `gsp-bench` modes (`latency`, the original
//! in-process harness, and `concurrency`, the separate-process load ramp).

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};

pub struct Stats {
    pub n: usize,
    pub mean: Duration,
    pub p50: Duration,
    pub p90: Duration,
    pub p99: Duration,
    pub p999: Duration,
    pub max: Duration,
}

impl Stats {
    pub fn of(mut v: Vec<Duration>) -> Self {
        v.sort_unstable();
        let n = v.len();
        let at = |q: f64| v[((n as f64 * q) as usize).min(n - 1)];
        let sum: Duration = v.iter().sum();
        Self {
            n,
            mean: sum / n as u32,
            p50: at(0.50),
            p90: at(0.90),
            p99: at(0.99),
            p999: at(0.999),
            max: v[n - 1],
        }
    }
}

pub fn us(d: Duration) -> f64 {
    d.as_secs_f64() * 1e6
}

pub fn free_addr() -> SocketAddr {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

// --------------------------------------------------------------------------
// In-process echo backends (both harness modes use the same backend; only
// the proxy under test is out-of-process in `concurrency` mode).
// --------------------------------------------------------------------------

pub async fn spawn_tcp_echo() -> SocketAddr {
    // A plain `TcpListener::bind` uses the OS default `listen()` backlog
    // (128 on Linux) — far too small once the `concurrency` mode ramps
    // hundreds of proxy-to-backend connects at once: SYNs queue up, some
    // get dropped/retransmitted, and the *backend accept queue* (not the
    // proxy) becomes the bottleneck, timing out the proxy's own
    // `connect_timeout_ms` and flapping the backend unhealthy. Bind with an
    // explicit large backlog instead, matching what `gsp-core`'s own
    // listener does for its bind sockets (`net::bind_reuseport_tcp`).
    let raw = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None).unwrap();
    raw.set_reuse_address(true).unwrap();
    raw.bind(&"127.0.0.1:0".parse::<SocketAddr>().unwrap().into())
        .unwrap();
    raw.listen(4096).unwrap();
    raw.set_nonblocking(true).unwrap();
    let l = TcpListener::from_std(raw.into()).unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            let _ = s.set_nodelay(true);
            tokio::spawn(async move {
                let mut buf = vec![0u8; 65536];
                loop {
                    match s.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if s.write_all(&buf[..n]).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }
    });
    addr
}

pub async fn spawn_udp_echo() -> SocketAddr {
    let s = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = s.local_addr().unwrap();
    tokio::spawn(async move {
        let mut buf = vec![0u8; 65536];
        while let Ok((n, peer)) = s.recv_from(&mut buf).await {
            let _ = s.send_to(&buf[..n], peer).await;
        }
    });
    addr
}

// --------------------------------------------------------------------------
// Round-trip loops.
// --------------------------------------------------------------------------

pub async fn tcp_rtt(target: SocketAddr, iters: usize, payload: usize) -> Result<Vec<Duration>> {
    let mut s = TcpStream::connect(target).await?;
    s.set_nodelay(true)?;
    let tx = vec![0xA5u8; payload];
    let mut rx = vec![0u8; payload];
    for _ in 0..1_000 {
        s.write_all(&tx).await?;
        s.read_exact(&mut rx).await?;
    }
    let mut out = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t = Instant::now();
        s.write_all(&tx).await?;
        s.read_exact(&mut rx).await?;
        out.push(t.elapsed());
    }
    Ok(out)
}

pub async fn udp_rtt(target: SocketAddr, iters: usize, payload: usize) -> Result<Vec<Duration>> {
    let s = UdpSocket::bind("127.0.0.1:0").await?;
    s.connect(target).await?;
    let tx = vec![0xA5u8; payload];
    let mut rx = vec![0u8; payload.max(1)];
    for _ in 0..1_000 {
        s.send(&tx).await?;
        let _ = s.recv(&mut rx).await?;
    }
    let mut out = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t = Instant::now();
        s.send(&tx).await?;
        let _ = s.recv(&mut rx).await?;
        out.push(t.elapsed());
    }
    Ok(out)
}

pub fn report(label: &str, direct: &Stats, proxy: &Stats) -> (f64, f64) {
    let add_p50 = us(proxy.p50).max(0.0) - us(direct.p50);
    let add_p99 = us(proxy.p99) - us(direct.p99);
    println!("\n{label}  ({} samples)", proxy.n);
    println!(
        "  {:<8} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9}",
        "", "mean", "p50", "p90", "p99", "p99.9", "max"
    );
    let row = |name: &str, s: &Stats| {
        println!(
            "  {:<8} {:>8.1}µ {:>8.1}µ {:>8.1}µ {:>8.1}µ {:>8.1}µ {:>8.1}µ",
            name,
            us(s.mean),
            us(s.p50),
            us(s.p90),
            us(s.p99),
            us(s.p999),
            us(s.max)
        );
    };
    row("direct", direct);
    row("proxy", proxy);
    println!("  added    p50 {add_p50:+.1}µs   p99 {add_p99:+.1}µs");
    (add_p50, add_p99)
}

pub fn rss_kib() -> Option<u64> {
    proc_field(std::process::id(), "VmRSS:")
}

/// Read a `key:` field (e.g. `"VmRSS:"`) from `/proc/<pid>/status`, parsing
/// the first whitespace-separated token after it as `u64` (works for the
/// `<n> kB` shaped fields; `Threads:` has no unit and still parses fine).
pub fn proc_field(pid: u32, key: &str) -> Option<u64> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    s.lines()
        .find_map(|l| l.strip_prefix(key))
        .and_then(|v| v.split_whitespace().next())
        .and_then(|n| n.parse().ok())
}

/// Count open file descriptors for `pid` via `/proc/<pid>/fd` — a direct
/// read of the proxy process's own socket/fd usage now that it's a real,
/// separate process (`concurrency` mode) rather than sharing this
/// harness's own fd table.
pub fn open_fd_count(pid: u32) -> Option<usize> {
    std::fs::read_dir(format!("/proc/{pid}/fd"))
        .ok()
        .map(std::iter::Iterator::count)
}
