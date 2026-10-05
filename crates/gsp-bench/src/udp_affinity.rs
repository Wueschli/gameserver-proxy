//! UDP session-open / affinity harness — the load test behind issue #56
//! (retire the UDP sticky table, which `consistent_hash` replaced).
//!
//! UDP affinity now comes only from the pool's balancer: a `consistent_hash`
//! pool pins a client to a backend with a rendezvous hash, so a client whose
//! idle session was evicted lands on the same backend when it comes back. A
//! `round_robin` pool has no affinity at all and serves as the baseline (the
//! per-worker sticky table that used to sit in front of it retained only
//! ~34% with 4 workers, see the README). This mode measures:
//!
//! 1. **Cold wave** — `--keys` distinct clients (one loopback source IP each,
//!    `127.x.y.z`) open a session at `--rate` sessions/s against a real,
//!    separate `gsp` process; reports achieved opens/s, open-latency
//!    percentiles, the proxy's RSS / fds, and the backend spread.
//! 2. **Drain** — wait out `--idle-sec` so every session is evicted, then
//!    sample the proxy's RSS again: what is left is the per-worker state that
//!    outlives sessions (allocator slack).
//! 3. **Warm wave** — the *same* clients come back, in a shuffled order; reports the share that
//!    lands on the same backend as before (**retention**) and the open
//!    latency again.
//!
//! Run it once per candidate configuration (`--balancer round_robin` is the
//! no-affinity baseline, `--balancer consistent_hash` the real thing), and
//! A/B two proxy builds with `--gsp-bin`. Loopback, one host — it measures
//! the proxy's own per-open cost, not a NIC or a kernel under real load.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::net::UdpSocket;
use tokio::sync::Semaphore;

use crate::common::{self, free_addr, spawn_tcp_echo, us, Stats};
use crate::concurrency::{build_gsp_release, write_config, ProxyProcess};

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum Balancer {
    RoundRobin,
    ConsistentHash,
}

impl Balancer {
    fn label(self) -> &'static str {
        match self {
            Balancer::RoundRobin => "round_robin",
            Balancer::ConsistentHash => "consistent_hash",
        }
    }
}

pub struct Args {
    /// Distinct clients (one loopback source IP each).
    pub keys: usize,
    /// Target session opens per second, per wave.
    pub rate: usize,
    pub backends: usize,
    /// Pool `idle_timeout_sec`; sessions are evicted this long after their
    /// last datagram (plus up to two 1 s wheel ticks).
    pub idle_sec: u64,
    pub balancer: Balancer,
    /// `src_ip` or `src_ip_port` (the `consistent_hash` pool's `hash_on`).
    pub hash_on: String,
    pub workers: usize,
    /// Use this `gsp` binary instead of building `target/release/gsp`.
    pub gsp_bin: Option<PathBuf>,
}

/// Minimum retention (warm wave on the same backend) the run reports as stable.
const RETENTION_OK_PCT: f64 = 99.0;
/// Concurrent in-flight opens from the harness, bounding its own fd use.
const CLIENT_INFLIGHT: usize = 2_000;

pub async fn run(args: &Args) -> Result<bool> {
    anyhow::ensure!(
        args.backends >= 1 && args.backends < 256,
        "--backends 1..255"
    );
    anyhow::ensure!(
        args.keys >= 1 && args.keys < (1 << 24) - 256,
        "--keys too large"
    );
    let gsp_bin = match &args.gsp_bin {
        Some(p) => p.clone(),
        None => {
            println!("building gsp --release …");
            build_gsp_release()?
        }
    };

    let backends = spawn_tagged_backends(args.backends).await;
    let udp_addr = free_addr();
    let ready_addr = free_addr(); // TCP readiness probe, see ProxyProcess::wait_ready
    let tcp_backend = spawn_tcp_echo().await;
    let targets = backends
        .iter()
        .map(|b| format!("\"{b}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let (workers, idle, hash_on) = (args.workers, args.idle_sec, &args.hash_on);
    let balancer = match args.balancer {
        Balancer::RoundRobin => "round_robin".to_string(),
        Balancer::ConsistentHash => format!("consistent_hash\n    hash_on: {hash_on}"),
    };
    let cfg_path = write_config(&format!(
        "settings:\n  workers: {workers}\n\
         pools:\n  - name: p\n    targets: [{targets}]\n    balancer: {balancer}\n    idle_timeout_sec: {idle}\n\
         \x20 - {{ name: ready, targets: [\"{tcp_backend}\"] }}\n\
         listeners:\n  - {{ name: l, bind: \"{udp_addr}\", protocol: udp, pool: p }}\n\
         \x20 - {{ name: ready, bind: \"{ready_addr}\", protocol: tcp, pool: ready }}\n"
    ))?;
    let mut proxy = ProxyProcess::spawn(&gsp_bin, cfg_path)?;
    proxy
        .wait_ready(ready_addr, Duration::from_secs(10))
        .await?;
    let pid = proxy.pid();

    println!(
        "UDP session-open / affinity — proxy pid {pid} on {udp_addr}\n\
         balancer {}, hash_on {hash_on}, {} backends, {} keys @ {}/s, idle {}s, {} proxy worker(s)\n\
         gsp: {}",
        args.balancer.label(),
        args.backends,
        args.keys,
        args.rate,
        args.idle_sec,
        args.workers,
        gsp_bin.display()
    );

    let rss = || common::proc_field(pid, "VmRSS:").map(|k| k as f64 / 1024.0);
    let fds = || common::open_fd_count(pid);
    let fmt = |v: Option<f64>| v.map_or("?".into(), |v| format!("{v:.1}"));
    let rss_idle = rss();

    let in_order: Vec<usize> = (0..args.keys).collect();
    let cold = wave(udp_addr, args, &in_order).await;
    let (rss_cold, fds_cold) = (rss(), fds());
    // Eviction: idle timeout, one wheel tick of slack, one more for the wheel
    // slot rounding.
    tokio::time::sleep(Duration::from_secs(args.idle_sec + 3)).await;
    let (rss_drained, fds_drained) = (rss(), fds());
    let warm = wave(udp_addr, args, &shuffled(args.keys)).await;
    let (rss_warm, _) = (rss(), fds());

    let mut same = 0usize;
    let mut compared = 0usize;
    for (k, b) in &cold.backend_of {
        if let Some(b2) = warm.backend_of.get(k) {
            compared += 1;
            same += usize::from(b == b2);
        }
    }
    let retention = if compared == 0 {
        0.0
    } else {
        same as f64 * 100.0 / compared as f64
    };

    println!(
        "\n{:<18} {:>9} {:>9} {:>9} {:>9} {:>9} {:>8} {:>8}",
        "wave", "opens/s", "p50", "p99", "p99.9", "max", "failed", "spread"
    );
    for (name, w) in [("cold (first open)", &cold), ("warm (re-open)", &warm)] {
        let s = &w.stats;
        println!(
            "{:<18} {:>9.0} {:>8.0}µ {:>8.0}µ {:>8.0}µ {:>8.0}µ {:>8} {:>8}",
            name,
            w.ok as f64 / w.secs,
            us(s.p50),
            us(s.p99),
            us(s.p999),
            us(s.max),
            w.failed,
            w.spread(args.backends),
        );
    }
    println!(
        "\nproxy RSS MiB: idle {}  after cold wave {}  after drain {}  after warm wave {}",
        fmt(rss_idle),
        fmt(rss_cold),
        fmt(rss_drained),
        fmt(rss_warm)
    );
    println!(
        "proxy fds: after cold wave {}  after drain {}",
        fds_cold.map_or("?".into(), |v| v.to_string()),
        fds_drained.map_or("?".into(), |v| v.to_string())
    );
    if let (Some(a), Some(b)) = (rss_idle, rss_drained) {
        println!(
            "state left after every session is evicted: {:+.1} MiB (~{:.0} B/key)",
            b - a,
            (b - a) * 1024.0 * 1024.0 / args.keys as f64
        );
    }
    let ok = retention >= RETENTION_OK_PCT;
    println!(
        "affinity retention (re-open on the same backend): {same}/{compared} = {retention:.1}%  → {}",
        if ok { "STABLE" } else { "LOST" }
    );
    println!(
        "(loopback, single host; compare --balancer round_robin with \
         --balancer consistent_hash, see udp_affinity.rs)"
    );
    proxy.kill();
    Ok(ok)
}

struct Wave {
    secs: f64,
    ok: usize,
    failed: usize,
    stats: Stats,
    /// client key → backend index that answered.
    backend_of: HashMap<usize, u8>,
    per_backend: Vec<usize>,
}

impl Wave {
    /// `min..max` share of opens per backend, as a percent of the successful opens.
    fn spread(&self, n: usize) -> String {
        if self.ok == 0 || n == 0 {
            return "-".into();
        }
        let pct = |c: usize| c as f64 * 100.0 / self.ok as f64;
        let min = self.per_backend.iter().copied().min().unwrap_or(0);
        let max = self.per_backend.iter().copied().max().unwrap_or(0);
        format!("{:.0}-{:.0}%", pct(min), pct(max))
    }
}

/// Open one session for each key at `args.rate`, collecting latency and the
/// answering backend.
async fn wave(proxy: SocketAddr, args: &Args, order: &[usize]) -> Wave {
    let sem = Arc::new(Semaphore::new(CLIENT_INFLIGHT));
    let tick = Duration::from_millis(10);
    let per_tick = (args.rate / 100).max(1);
    let mut ticker = tokio::time::interval(tick);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);
    let start = Instant::now();
    let mut handles = Vec::with_capacity(args.keys);
    let mut next = 0usize;
    while next < args.keys {
        ticker.tick().await;
        for _ in 0..per_tick.min(args.keys - next) {
            let k = order[next];
            next += 1;
            let sem = sem.clone();
            handles.push(tokio::spawn(async move {
                let _permit = sem.acquire_owned().await.ok()?;
                open_one(proxy, k).await.map(|(b, d)| (k, b, d))
            }));
        }
    }
    let mut lat = Vec::with_capacity(args.keys);
    let mut backend_of = HashMap::with_capacity(args.keys);
    let mut per_backend = vec![0usize; args.backends];
    let mut failed = 0usize;
    for h in handles {
        match h.await {
            Ok(Some((k, b, d))) => {
                lat.push(d);
                backend_of.insert(k, b);
                if let Some(c) = per_backend.get_mut(b as usize) {
                    *c += 1;
                }
            }
            _ => failed += 1,
        }
    }
    let secs = start.elapsed().as_secs_f64();
    let ok = lat.len();
    Wave {
        secs,
        ok,
        failed,
        stats: Stats::of(if lat.is_empty() {
            vec![Duration::ZERO]
        } else {
            lat
        }),
        backend_of,
        per_backend,
    }
}

/// `0..n` in a fixed pseudo-random order (xorshift Fisher-Yates), so the warm
/// wave does not revisit keys in the order a wholesale-cleared table filled.
fn shuffled(n: usize) -> Vec<usize> {
    let mut v: Vec<usize> = (0..n).collect();
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    for i in (1..n).rev() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.swap(i, (x % (i as u64 + 1)) as usize);
    }
    v
}

/// Client `k`'s source address: one loopback IP per key (the whole `127/8`
/// is local), starting past `127.0.0.x` so it never collides with a backend.
fn client_ip(k: usize) -> Ipv4Addr {
    let n = (k + 256) as u32;
    Ipv4Addr::new(127, (n >> 16) as u8, (n >> 8) as u8, n as u8)
}

/// One datagram through the proxy from key `k`; the reply's first byte is the
/// answering backend's index. One retry covers a burst dropped by a full
/// socket buffer (counted as latency, not hidden).
async fn open_one(proxy: SocketAddr, k: usize) -> Option<(u8, Duration)> {
    let s = UdpSocket::bind((client_ip(k), 0)).await.ok()?;
    s.connect(proxy).await.ok()?;
    let t = Instant::now();
    let mut buf = [0u8; 16];
    for _ in 0..2 {
        s.send(b"ping").await.ok()?;
        if let Ok(Ok(n)) = tokio::time::timeout(Duration::from_millis(500), s.recv(&mut buf)).await
        {
            if n >= 1 {
                return Some((buf[0], t.elapsed()));
            }
        }
    }
    None
}

/// `n` UDP echo backends; each reply is `[its index] ++ payload`, so a client
/// can tell which backend it was sent to.
async fn spawn_tagged_backends(n: usize) -> Vec<SocketAddr> {
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let s = UdpSocket::bind("127.0.0.1:0")
            .await
            .context("binding a backend")
            .unwrap();
        out.push(s.local_addr().unwrap());
        tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            let mut reply = Vec::with_capacity(2049);
            while let Ok((len, peer)) = s.recv_from(&mut buf).await {
                reply.clear();
                reply.push(i as u8);
                reply.extend_from_slice(&buf[..len]);
                let _ = s.send_to(&reply, peer).await;
            }
        });
    }
    out
}
