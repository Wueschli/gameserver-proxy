//! Load / latency harness for the NFR targets in `docs/01-requirements.md`.
//!
//! Three modes:
//!
//! - **`latency`** (default) — single-host, *in-process* harness (client,
//!   proxy and backend all share this process's tokio runtime). Measures
//!   **N1 / N2** — *added* request/response latency the proxy introduces:
//!   p50 < 0.5 ms, p99 < 2 ms — as `proxy_rtt − direct_rtt` over many
//!   sequential round-trips, optionally under load from N extra busy
//!   connections. Also reports single-stream throughput (informational, not
//!   N3) and idle-RSS-per-connection (informational, a loose proxy for N8).
//! - **`concurrency`** — a real, *separate-process* proxy (the actual `wayhouse`
//!   binary, built `--release` and spawned as a child process) driven by a
//!   ramp of increasingly many concurrently-held connections, reporting the
//!   proxy child's own RSS / open-fd count and the added-latency percentiles
//!   of a probe connection at each step. See `concurrency.rs` for why this
//!   exists and what it does and doesn't validate.
//!
//! - **`udp-affinity`** — session-open rate, open latency, residual state and
//!   affinity retention against a real `wayhouse` process; the load test behind
//!   issue #56 (UDP sticky table vs `consistent_hash`). See `udp_affinity.rs`.
//!
//! None of these modes is NFR N3 (≥ 20 Gbit/s aggregate on real NICs — loopback
//! bandwidth exceeds this, so a local "pass" would be meaningless) or N9
//! (HA — needs real hosts/network). See `docs/06-operations-observability.md`
//! and this crate's `README.md`.

mod common;
mod concurrency;
mod udp_affinity;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, ValueEnum};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use common::{free_addr, report, rss_kib, spawn_tcp_echo, spawn_udp_echo, tcp_rtt, udp_rtt, Stats};
use wayhouse_core::{Runtime, Snapshot};

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum Proto {
    Tcp,
    Udp,
    Both,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum Mode {
    /// In-process added-latency harness (NFR N1/N2). Default.
    Latency,
    /// Separate-process concurrency ramp. See `concurrency.rs`.
    Concurrency,
    /// UDP session-open / affinity load test (issue #56). See `udp_affinity.rs`.
    UdpAffinity,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum BalancerArg {
    RoundRobin,
    ConsistentHash,
}

#[derive(Parser)]
#[command(about = "wayhouse latency / load harness (NFR N1/N2, + a concurrency ramp)")]
struct Args {
    /// Which harness to run.
    #[arg(long, value_enum, default_value = "latency")]
    mode: Mode,
    /// Which transport(s) to measure.
    #[arg(long, value_enum, default_value = "both")]
    protocol: Proto,
    /// Timed round-trips per measurement (`latency` mode; also the probe
    /// sample count per step in `concurrency` mode).
    #[arg(long, default_value_t = 20_000)]
    iterations: usize,
    /// Request/response payload size in bytes.
    #[arg(long, default_value_t = 64)]
    payload: usize,
    /// `latency` mode: extra busy connections held open during the
    /// measurement (adds runtime contention). The timed connection is
    /// separate.
    #[arg(long, default_value_t = 0)]
    connections: usize,
    /// Worker threads for the proxy runtime (0 = one per core).
    #[arg(long, default_value_t = 1)]
    workers: usize,
    /// `concurrency` mode: comma-separated connection-count steps to ramp
    /// through (each step's connections stay open through the later steps).
    /// Capped by the loopback ephemeral-port range (~28k on this host,
    /// `cat /proc/sys/net/ipv4/ip_local_port_range`) — see `concurrency.rs`.
    #[arg(long, default_value = "1000,5000,10000,20000")]
    steps: String,
    /// `udp-affinity` mode: distinct clients (one loopback source IP each).
    /// Above 65536 the per-worker sticky table overflows.
    #[arg(long, default_value_t = 100_000)]
    keys: usize,
    /// `udp-affinity` mode: target session opens per second.
    #[arg(long, default_value_t = 4_000)]
    rate: usize,
    /// `udp-affinity` mode: number of backends in the pool.
    #[arg(long, default_value_t = 8)]
    backends: usize,
    /// `udp-affinity` mode: pool `idle_timeout_sec`.
    #[arg(long, default_value_t = 1)]
    idle_sec: u64,
    /// `udp-affinity` mode: pool balancer. `round_robin` exercises the
    /// listener's sticky table, `consistent_hash` its replacement.
    #[arg(long, value_enum, default_value = "round-robin")]
    balancer: BalancerArg,
    /// `udp-affinity` mode: `src_ip` or `src_ip_port`.
    #[arg(long, default_value = "src_ip")]
    hash_on: String,
    /// `udp-affinity` mode: spawn this `wayhouse` binary instead of building
    /// `target/release/wayhouse` (A/B two builds).
    #[arg(long)]
    wayhouse_bin: Option<std::path::PathBuf>,
    /// Exit non-zero if an NFR target is missed (`udp-affinity`: if affinity
    /// retention falls below 99%).
    #[arg(long)]
    strict: bool,
}

fn parse_steps(s: &str) -> Result<Vec<usize>> {
    s.split(',')
        .map(|p| p.trim().parse::<usize>().map_err(Into::into))
        .collect()
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
                        tokio::io::AsyncReadExt::read_exact(&mut s, &mut rx).await
                    } => { if r.is_err() { break; } }
                }
            }
        });
    }
}

async fn throughput(target: SocketAddr, mib: usize) -> Result<f64> {
    use tokio::io::AsyncReadExt;
    let mut s = TcpStream::connect(target).await?;
    s.set_nodelay(true)?;
    let total = mib * 1024 * 1024;
    let chunk = vec![0u8; 256 * 1024];
    let mut rx = vec![0u8; 256 * 1024];
    let t = std::time::Instant::now();
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

async fn run_latency(args: &Args) -> Result<bool> {
    // N1 = added p50 < 500µs, N2 = added p99 < 2000µs.
    const N1_US: f64 = 500.0;
    const N2_US: f64 = 2000.0;
    let mut ok = true;

    let want_tcp = matches!(args.protocol, Proto::Tcp | Proto::Both);
    let want_udp = matches!(args.protocol, Proto::Udp | Proto::Both);

    if want_tcp {
        let backend = spawn_tcp_echo().await;
        let proxy_addr = free_addr();
        let cfg = wayhouse_config::parse_str(&format!(
            "pools:\n  - {{ name: p, targets: [\"{backend}\"] }}\n\
             listeners:\n  - {{ name: l, bind: \"{proxy_addr}\", protocol: tcp, pool: p }}\n"
        ))?;
        let rt = Runtime::start(Snapshot::from_config(&cfg), Arc::default(), args.workers);
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
        let cfg = wayhouse_config::parse_str(&format!(
            "pools:\n  - {{ name: p, targets: [\"{backend}\"], health_check: {{ type: none }} }}\n\
             listeners:\n  - {{ name: l, bind: \"{proxy_addr}\", protocol: udp, pool: p }}\n"
        ))?;
        let rt = Runtime::start(Snapshot::from_config(&cfg), Arc::default(), args.workers);
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
    let steps = parse_steps(&args.steps)?;
    let ok = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async {
            match args.mode {
                Mode::Latency => run_latency(&args).await,
                Mode::Concurrency => concurrency::run(&args_to_concurrency(&args, steps)).await,
                Mode::UdpAffinity => {
                    udp_affinity::run(&udp_affinity::Args {
                        keys: args.keys,
                        rate: args.rate,
                        backends: args.backends,
                        idle_sec: args.idle_sec,
                        balancer: match args.balancer {
                            BalancerArg::RoundRobin => udp_affinity::Balancer::RoundRobin,
                            BalancerArg::ConsistentHash => udp_affinity::Balancer::ConsistentHash,
                        },
                        hash_on: args.hash_on.clone(),
                        workers: args.workers,
                        wayhouse_bin: args.wayhouse_bin.clone(),
                    })
                    .await
                }
            }
        })?;
    if args.strict && !ok {
        std::process::exit(1);
    }
    Ok(())
}

fn args_to_concurrency(args: &Args, steps: Vec<usize>) -> concurrency::Args {
    concurrency::Args {
        protocol: match args.protocol {
            Proto::Tcp => concurrency::Proto::Tcp,
            Proto::Udp => concurrency::Proto::Udp,
            Proto::Both => concurrency::Proto::Both,
        },
        steps,
        probe_iterations: args.iterations.clamp(200, 2_000),
        payload: args.payload,
        workers: args.workers,
    }
}
