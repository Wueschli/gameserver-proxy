//! Separate-process concurrency ramp — the part of the load story the
//! original `latency` mode structurally can't cover.
//!
//! `latency` mode runs the client, the proxy's `Runtime`, and the backend
//! all inside *this one process's* tokio runtime. That's fine for measuring
//! per-request overhead, but it hides everything that only shows up once the
//! proxy is a genuinely separate OS process under real concurrent load:
//! accept-queue backpressure, per-fd bookkeeping cost, allocator behaviour
//! with thousands of live connections, and — because it's now a distinct
//! process — its *own*, directly-observable RSS and open-fd count instead of
//! a shared number polluted by the harness's own client-side sockets.
//!
//! This mode builds the real `gsp` binary (`--release` — a debug build's
//! overhead isn't representative of anything), spawns it as a child process
//! against a config generated for the run, and ramps a real client-held
//! connection count through `--steps`, sampling the proxy child's RSS / fd
//! count and a probe connection's added-latency percentiles at each step.
//!
//! **What this does and doesn't validate** (see `crates/gsp-bench/README.md`
//! and `docs/06-operations-observability.md` for the full picture): it's
//! still loopback on one host, so it says nothing about NFR N3 (a real NIC's
//! aggregate bandwidth) or N9 (real multi-host HA). And the *count* it can
//! reach is capped well below the N4/N5 targets (500k / 1M) by the client's
//! own ephemeral-port range for outbound connections to one destination
//! (`cat /proc/sys/net/ipv4/ip_local_port_range` — typically ~28k on Linux
//! without extra source addresses). What it *does* give: a real separate
//! proxy process, under real concurrent load, at a scale far beyond the
//! in-process harness's usual single measured connection — enough to reveal
//! most classes of concurrency bug even though it can't reach the full NFR
//! numbers.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::io::AsyncReadExt;
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::{Notify, Semaphore};

use crate::common::{self, free_addr, spawn_tcp_echo, spawn_udp_echo, tcp_rtt, udp_rtt, us, Stats};

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum Proto {
    Tcp,
    Udp,
    Both,
}

pub struct Args {
    pub protocol: Proto,
    /// Cumulative connection-count steps (each step's connections stay open
    /// through later steps).
    pub steps: Vec<usize>,
    /// Probe round-trips sampled at each step (kept short — this runs once
    /// per step, not once for the whole harness).
    pub probe_iterations: usize,
    pub payload: usize,
    /// Passed through as the spawned proxy's `settings.workers`.
    pub workers: usize,
}

const N1_US: f64 = 500.0;
const N2_US: f64 = 2000.0;
/// How many outbound connect/session-open attempts run concurrently during
/// ramp-up — bounds the instantaneous connect storm so it doesn't just
/// measure SYN-backlog drops.
const RAMP_CONCURRENCY: usize = 200;

pub async fn run(args: &Args) -> Result<bool> {
    println!("building gsp --release …");
    let gsp_bin = build_gsp_release()?;

    let want_tcp = matches!(args.protocol, Proto::Tcp | Proto::Both);
    let want_udp = matches!(args.protocol, Proto::Udp | Proto::Both);
    let mut ok = true;

    if want_tcp {
        ok &= tcp_ramp(&gsp_bin, args).await?;
    }
    if want_udp {
        ok &= udp_ramp(&gsp_bin, args).await?;
    }

    println!(
        "\nConcurrency ramp: NFR N1/N2 held under load: {}",
        if ok { "PASS" } else { "MISS" }
    );
    println!(
        "(separate-process proxy, loopback single host — see concurrency.rs doc comment for \
         what this does and doesn't prove about N3/N4/N5/N9.)"
    );
    Ok(ok)
}

// --------------------------------------------------------------------------
// Building and running the real proxy binary.
// --------------------------------------------------------------------------

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root must exist")
}

pub(crate) fn build_gsp_release() -> Result<PathBuf> {
    let status = std::process::Command::new("cargo")
        .args(["build", "--release", "-p", "gsp"])
        .current_dir(workspace_root())
        .status()
        .context("running `cargo build --release -p gsp` — is cargo on PATH?")?;
    anyhow::ensure!(status.success(), "building the gsp binary failed");
    let bin = workspace_root().join("target/release/gsp");
    anyhow::ensure!(
        bin.is_file(),
        "expected {} to exist after build",
        bin.display()
    );
    Ok(bin)
}

pub(crate) struct ProxyProcess {
    child: std::process::Child,
    config_path: PathBuf,
}

impl ProxyProcess {
    pub(crate) fn spawn(gsp_bin: &Path, config_path: PathBuf) -> Result<Self> {
        let child = std::process::Command::new(gsp_bin)
            .arg("--config")
            .arg(&config_path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .context("spawning the gsp binary")?;
        Ok(Self { child, config_path })
    }

    pub(crate) fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Poll a TCP connect to `ready_addr` until it succeeds or `timeout`
    /// elapses. Every generated config includes a dedicated TCP "ready"
    /// listener (even for a UDP-only ramp) exactly so this works uniformly.
    pub(crate) async fn wait_ready(&self, ready_addr: SocketAddr, timeout: Duration) -> Result<()> {
        let start = Instant::now();
        loop {
            if TcpStream::connect(ready_addr).await.is_ok() {
                return Ok(());
            }
            anyhow::ensure!(
                start.elapsed() <= timeout,
                "proxy (pid {}) did not become ready on {ready_addr} within {timeout:?}",
                self.pid()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    pub(crate) fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.config_path);
    }
}

impl Drop for ProxyProcess {
    /// Belt-and-braces: an early `?` return (e.g. `wait_ready` timing out)
    /// must not leak the child process or its temp config file.
    fn drop(&mut self) {
        self.kill();
    }
}

pub(crate) fn write_config(yaml: &str) -> Result<PathBuf> {
    let path = std::env::temp_dir().join(format!(
        "gsp-bench-concurrency-{}-{}.yaml",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::write(&path, yaml).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

// --------------------------------------------------------------------------
// Holding connections / sessions open under a stop signal.
// --------------------------------------------------------------------------

async fn hold_tcp(
    target: SocketAddr,
    n: usize,
    stop: Arc<Notify>,
    established: Arc<AtomicUsize>,
    failed: Arc<AtomicUsize>,
) {
    let sem = Arc::new(Semaphore::new(RAMP_CONCURRENCY));
    for _ in 0..n {
        let sem = sem.clone();
        let stop = stop.clone();
        let established = established.clone();
        let failed = failed.clone();
        tokio::spawn(async move {
            let permit = sem.acquire_owned().await.unwrap();
            match TcpStream::connect(target).await {
                Ok(mut s) => {
                    drop(permit);
                    established.fetch_add(1, Ordering::Relaxed);
                    // We never write on a held connection, so a successful
                    // read here can only mean the peer closed it — that's
                    // an unexpected death, not the intended `stop` teardown.
                    // `established` therefore tracks *currently live* held
                    // connections, not just "was connected at some point".
                    let mut probe_buf = [0u8; 1];
                    tokio::select! {
                        _ = stop.notified() => {}
                        _ = s.read(&mut probe_buf) => {
                            established.fetch_sub(1, Ordering::Relaxed);
                            failed.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    drop(s);
                }
                Err(_) => {
                    drop(permit);
                    failed.fetch_add(1, Ordering::Relaxed);
                }
            }
        });
    }
}

async fn hold_udp(
    target: SocketAddr,
    n: usize,
    stop: Arc<Notify>,
    established: Arc<AtomicUsize>,
    failed: Arc<AtomicUsize>,
) {
    let sem = Arc::new(Semaphore::new(RAMP_CONCURRENCY));
    for _ in 0..n {
        let sem = sem.clone();
        let stop = stop.clone();
        let established = established.clone();
        let failed = failed.clone();
        tokio::spawn(async move {
            let permit = sem.acquire_owned().await.unwrap();
            let opened = async {
                let s = UdpSocket::bind("127.0.0.1:0").await?;
                s.connect(target).await?;
                s.send(b"ping").await?;
                let mut buf = [0u8; 16];
                tokio::time::timeout(Duration::from_millis(300), s.recv(&mut buf)).await??;
                anyhow::Ok(s)
            }
            .await;
            drop(permit);
            match opened {
                Ok(s) => {
                    established.fetch_add(1, Ordering::Relaxed);
                    stop.notified().await;
                    drop(s);
                }
                Err(_) => {
                    failed.fetch_add(1, Ordering::Relaxed);
                }
            }
        });
    }
}

// --------------------------------------------------------------------------
// The ramps.
// --------------------------------------------------------------------------

fn print_header() {
    println!(
        "\n{:>10} {:>10} {:>8} {:>9} {:>6}  {:>9} {:>9}  N1/N2",
        "held", "connected", "failed", "rss MiB", "fds", "add p50", "add p99"
    );
}

struct StepReport {
    held: usize,
    connected: usize,
    failed: usize,
    rss_mib: Option<f64>,
    fds: Option<usize>,
    add_p50: f64,
    add_p99: f64,
    pass: bool,
}

fn print_row(r: &StepReport) {
    println!(
        "{:>10} {:>10} {:>8} {:>9} {:>6}  {:>8.1}µ {:>8.1}µ  {}",
        r.held,
        r.connected,
        r.failed,
        r.rss_mib
            .map(|v| format!("{v:.1}"))
            .unwrap_or_else(|| "?".into()),
        r.fds.map(|v| v.to_string()).unwrap_or_else(|| "?".into()),
        r.add_p50,
        r.add_p99,
        if r.pass { "PASS" } else { "MISS" },
    );
}

async fn tcp_ramp(gsp_bin: &Path, args: &Args) -> Result<bool> {
    let backend = spawn_tcp_echo().await;
    let tcp_addr = free_addr();
    let admin_addr = free_addr();
    let workers = args.workers;
    let cfg_path = write_config(&format!(
        "settings:\n  workers: {workers}\n  admin:\n    listen: \"{admin_addr}\"\npools:\n  - {{ name: p, targets: [\"{backend}\"], idle_timeout_sec: 600 }}\nlisteners:\n  - {{ name: l, bind: \"{tcp_addr}\", protocol: tcp, pool: p }}\n"
    ))?;

    let mut proxy = ProxyProcess::spawn(gsp_bin, cfg_path)?;
    proxy.wait_ready(tcp_addr, Duration::from_secs(10)).await?;
    println!(
        "\nTCP concurrency ramp — proxy pid {} on {tcp_addr} (admin {admin_addr})",
        proxy.pid()
    );

    let stop = Arc::new(Notify::new());
    let established = Arc::new(AtomicUsize::new(0));
    let failed = Arc::new(AtomicUsize::new(0));
    let mut held = 0usize;
    let mut ok = true;

    print_header();
    for &target in &args.steps {
        if target <= held {
            continue;
        }
        let to_add = target - held;
        hold_tcp(
            tcp_addr,
            to_add,
            stop.clone(),
            established.clone(),
            failed.clone(),
        )
        .await;
        // Let ramp-up settle: proportional to how many we just asked for,
        // floored so small steps don't get shortchanged.
        tokio::time::sleep(Duration::from_millis(500 + (to_add as u64 / 5))).await;
        held = target;

        let direct = Stats::of(
            tcp_rtt(backend, args.probe_iterations, args.payload)
                .await
                .context("direct probe (backend)")?,
        );
        let via_proxy = Stats::of(
            tcp_rtt(tcp_addr, args.probe_iterations, args.payload)
                .await
                .context("proxy probe")?,
        );
        let add_p50 = us(via_proxy.p50) - us(direct.p50);
        let add_p99 = us(via_proxy.p99) - us(direct.p99);
        let pass = add_p50 < N1_US && add_p99 < N2_US;
        ok &= pass;

        let rss_mib = common::proc_field(proxy.pid(), "VmRSS:").map(|k| k as f64 / 1024.0);
        let fds = common::open_fd_count(proxy.pid());
        print_row(&StepReport {
            held,
            connected: established.load(Ordering::Relaxed),
            failed: failed.load(Ordering::Relaxed),
            rss_mib,
            fds,
            add_p50,
            add_p99,
            pass,
        });
    }

    stop.notify_waiters();
    tokio::time::sleep(Duration::from_millis(200)).await;
    proxy.kill();
    Ok(ok)
}

async fn udp_ramp(gsp_bin: &Path, args: &Args) -> Result<bool> {
    let backend = spawn_udp_echo().await;
    let udp_addr = free_addr();
    let ready_addr = free_addr(); // TCP readiness probe, see ProxyProcess::wait_ready
    let admin_addr = free_addr();
    let tcp_backend = spawn_tcp_echo().await;
    let workers = args.workers;
    let cfg_path = write_config(&format!(
        "settings:\n  workers: {workers}\n  admin:\n    listen: \"{admin_addr}\"\npools:\n  - {{ name: p, targets: [\"{backend}\"], idle_timeout_sec: 600 }}\n  - {{ name: ready, targets: [\"{tcp_backend}\"] }}\nlisteners:\n  - {{ name: l, bind: \"{udp_addr}\", protocol: udp, pool: p }}\n  - {{ name: ready, bind: \"{ready_addr}\", protocol: tcp, pool: ready }}\n"
    ))?;

    let mut proxy = ProxyProcess::spawn(gsp_bin, cfg_path)?;
    proxy
        .wait_ready(ready_addr, Duration::from_secs(10))
        .await?;
    println!(
        "\nUDP concurrency ramp — proxy pid {} on {udp_addr} (admin {admin_addr})",
        proxy.pid()
    );

    let stop = Arc::new(Notify::new());
    let established = Arc::new(AtomicUsize::new(0));
    let failed = Arc::new(AtomicUsize::new(0));
    let mut held = 0usize;
    let mut ok = true;

    print_header();
    for &target in &args.steps {
        if target <= held {
            continue;
        }
        let to_add = target - held;
        hold_udp(
            udp_addr,
            to_add,
            stop.clone(),
            established.clone(),
            failed.clone(),
        )
        .await;
        tokio::time::sleep(Duration::from_millis(500 + (to_add as u64 / 10))).await;
        held = target;

        let direct = Stats::of(udp_rtt(backend, args.probe_iterations, args.payload).await?);
        let via_proxy = Stats::of(udp_rtt(udp_addr, args.probe_iterations, args.payload).await?);
        let add_p50 = us(via_proxy.p50) - us(direct.p50);
        let add_p99 = us(via_proxy.p99) - us(direct.p99);
        let pass = add_p50 < N1_US && add_p99 < N2_US;
        ok &= pass;

        let rss_mib = common::proc_field(proxy.pid(), "VmRSS:").map(|k| k as f64 / 1024.0);
        let fds = common::open_fd_count(proxy.pid());
        print_row(&StepReport {
            held,
            connected: established.load(Ordering::Relaxed),
            failed: failed.load(Ordering::Relaxed),
            rss_mib,
            fds,
            add_p50,
            add_p99,
            pass,
        });
    }

    stop.notify_waiters();
    tokio::time::sleep(Duration::from_millis(200)).await;
    proxy.kill();
    Ok(ok)
}
