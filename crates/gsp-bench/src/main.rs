//! Load / latency harness for the NFR targets in `docs/01-requirements.md`.
//!
//! What it measures on a single host (loopback):
//!
//! - **N1 / N2** — *added* request/response latency the proxy introduces:
//!   p50 < 0.5 ms, p99 < 2 ms. Measured as `proxy_rtt − direct_rtt` over many
//!   sequential round-trips on one connection, optionally under load from N
//!   extra busy connections.
//! - **throughput** (informational) — single-stream bidirectional MB/s, direct
//!   vs. through the proxy. Not NFR N3 (which is a ≥ 20 Gbit/s aggregate on real
//!   NICs; use `tcpkali` / `wrk2` on dedicated hosts for that).
//! - **idle memory** (informational, with `--connections ≥ 1000`) — this
//!   process's RSS growth per held-open idle proxy connection, a loose proxy for
//!   NFR N8.
//!
//! NFR N3 (throughput), N4/N5 (500k conns / 1M sessions) and N9 (HA) need
//! hardware and a real load generator and are out of scope here — see
//! `docs/06-operations-observability.md`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::{Parser, ValueEnum};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};

use gsp_core::{Runtime, Snapshot};

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum Proto {
    Tcp,
    Udp,
    Both,
}

#[derive(Parser)]
#[command(about = "gsp latency / load harness (NFR N1/N2)")]
struct Args {
    /// Which transport(s) to measure.
    #[arg(long, value_enum, default_value = "both")]
    protocol: Proto,
    /// Timed round-trips per measurement.
    #[arg(long, default_value_t = 20_000)]
    iterations: usize,
    /// Request/response payload size in bytes.
    #[arg(long, default_value_t = 64)]
    payload: usize,
    /// Extra busy connections held open during the measurement (adds runtime
    /// contention). The timed connection is separate.
    #[arg(long, default_value_t = 0)]
    connections: usize,
    /// Worker threads for the proxy runtime (0 = one per core).
    #[arg(long, default_value_t = 1)]
    workers: usize,
    /// Exit non-zero if an NFR target is missed.
    #[arg(long)]
    strict: bool,
}

struct Stats {
    n: usize,
    mean: Duration,
    p50: Duration,
    p90: Duration,
    p99: Duration,
    p999: Duration,
    max: Duration,
}

impl Stats {
    fn of(mut v: Vec<Duration>) -> Self {
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

fn us(d: Duration) -> f64 {
    d.as_secs_f64() * 1e6
}

fn free_addr() -> SocketAddr {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

// --------------------------------------------------------------------------
// Backends.
// --------------------------------------------------------------------------

async fn spawn_tcp_echo() -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
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

async fn spawn_udp_echo() -> SocketAddr {
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

async fn tcp_rtt(target: SocketAddr, iters: usize, payload: usize) -> Result<Vec<Duration>> {
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

async fn udp_rtt(target: SocketAddr, iters: usize, payload: usize) -> Result<Vec<Duration>> {
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

/// Hold `n` connections open, each looping request/response, until `stop`.
async fn tcp_load(target: SocketAddr, n: usize, stop: Arc<tokio::sync::Notify>) {
    for _ in 0..n {
        let stop = stop.clone();
        tokio::spawn(async move {
            let Ok(mut s) = TcpStream::connect(target).await else {
                return;
            };
            let _ = s.set_nodelay(true);
            let tx = [0xA5u8; 64];
            let mut rx = [0u8; 64];
            loop {
                tokio::select! {
                    _ = stop.notified() => break,
                    r = async {
                        s.write_all(&tx).await?;
                        s.read_exact(&mut rx).await
                    } => { if r.is_err() { break; } }
                }
            }
        });
    }
}

async fn throughput(target: SocketAddr, mib: usize) -> Result<f64> {
    let mut s = TcpStream::connect(target).await?;
    s.set_nodelay(true)?;
    let total = mib * 1024 * 1024;
    let chunk = vec![0u8; 256 * 1024];
    let mut rx = vec![0u8; 256 * 1024];
    let t = Instant::now();
    let (mut sent, mut recv) = (0usize, 0usize);
    while recv < total {
        if sent < total {
            let want = chunk.len().min(total - sent);
            s.write_all(&chunk[..want]).await?;
            sent += want;
        }
        let n = s.read(&mut rx).await?;
        if n == 0 {
            break;
        }
        recv += n;
    }
    let secs = t.elapsed().as_secs_f64();
    Ok((total as f64 * 2.0) / secs / (1024.0 * 1024.0)) // bidirectional MiB/s
}

// --------------------------------------------------------------------------

fn rss_kib() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    s.lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.split_whitespace().next())
        .and_then(|n| n.parse().ok())
}

fn report(label: &str, direct: &Stats, proxy: &Stats) -> (f64, f64) {
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

async fn run(args: &Args) -> Result<bool> {
    // N1 = added p50 < 500µs, N2 = added p99 < 2000µs.
    const N1_US: f64 = 500.0;
    const N2_US: f64 = 2000.0;
    let mut ok = true;

    let want_tcp = matches!(args.protocol, Proto::Tcp | Proto::Both);
    let want_udp = matches!(args.protocol, Proto::Udp | Proto::Both);

    if want_tcp {
        let backend = spawn_tcp_echo().await;
        let proxy_addr = free_addr();
        let cfg = gsp_config::parse_str(&format!(
            "pools:\n  - {{ name: p, targets: [\"{backend}\"] }}\n\
             listeners:\n  - {{ name: l, bind: \"{proxy_addr}\", protocol: tcp, pool: p }}\n"
        ))?;
        let rt = Runtime::start(
            Snapshot::from_config(&cfg),
            Default::default(),
            args.workers,
        );
        tokio::time::sleep(Duration::from_millis(200)).await;

        let stop = Arc::new(tokio::sync::Notify::new());
        if args.connections > 0 {
            tcp_load(proxy_addr, args.connections, stop.clone()).await;
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let rss0 = rss_kib();

        let direct = Stats::of(tcp_rtt(backend, args.iterations, args.payload).await?);
        let proxy = Stats::of(tcp_rtt(proxy_addr, args.iterations, args.payload).await?);
        let (p50, p99) = report("TCP request/response", &direct, &proxy);
        ok &= p50 < N1_US && p99 < N2_US;

        if args.connections >= 1_000 {
            if let (Some(a), Some(b)) = (rss0, rss_kib()) {
                println!(
                    "  idle RSS: +{} KiB over {} held connections (~{:.0} B/conn, incl. client)",
                    b.saturating_sub(a),
                    args.connections,
                    (b.saturating_sub(a) as f64 * 1024.0) / args.connections as f64
                );
            }
        }

        let d = throughput(backend, 512).await?;
        let p = throughput(proxy_addr, 512).await?;
        println!(
            "  throughput (1 stream, informational): direct {d:.0} MiB/s   proxy {p:.0} MiB/s"
        );

        stop.notify_waiters();
        rt.shutdown_with_grace(Duration::from_millis(200)).await;
    }

    if want_udp {
        let backend = spawn_udp_echo().await;
        let proxy_addr = free_addr();
        let cfg = gsp_config::parse_str(&format!(
            "pools:\n  - {{ name: p, targets: [\"{backend}\"] }}\n\
             listeners:\n  - {{ name: l, bind: \"{proxy_addr}\", protocol: udp, pool: p }}\n"
        ))?;
        let rt = Runtime::start(
            Snapshot::from_config(&cfg),
            Default::default(),
            args.workers,
        );
        tokio::time::sleep(Duration::from_millis(200)).await;

        let direct = Stats::of(udp_rtt(backend, args.iterations, args.payload).await?);
        let proxy = Stats::of(udp_rtt(proxy_addr, args.iterations, args.payload).await?);
        let (p50, p99) = report("UDP request/response", &direct, &proxy);
        ok &= p50 < N1_US && p99 < N2_US;

        rt.shutdown_with_grace(Duration::from_millis(200)).await;
    }

    println!(
        "\nNFR N1 (added p50 < {N1_US:.0}µs) / N2 (added p99 < {N2_US:.0}µs): {}",
        if ok { "PASS" } else { "MISS" }
    );
    println!(
        "(loopback, single host, {} worker(s); real N1/N2 need same-DC hardware. \
         N3/N4/N5 need a dedicated load generator.)",
        args.workers
    );
    Ok(ok)
}

fn main() -> Result<()> {
    let args = Args::parse();
    let ok = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(&args))?;
    if args.strict && !ok {
        std::process::exit(1);
    }
    Ok(())
}
