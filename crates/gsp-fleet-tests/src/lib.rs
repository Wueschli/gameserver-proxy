//! Slice 12 (`docs/08-roadmap.md` phase 10+11): fleet integration tests.
//!
//! Every other phase 10+11 slice was verified by a human standing up real
//! `gsp` / `gsp-controller` / `gsp-aggregator` / `gsp-ui` processes and
//! driving them over real HTTP (see `HANDOVER.md`'s slice notes). This crate
//! automates exactly that shape of test instead of inventing a new
//! in-process harness: `tests/fleet.rs` spawns the real debug binaries as
//! child processes on `127.0.0.1` with OS-assigned ports, the same way a
//! human (or `gsp-bench`'s `concurrency` mode) would from a shell.
//!
//! Deliberately **not** an in-process harness (mocking each binary's
//! `main`): the whole point of slice 12 is to catch the class of bug slices
//! 11e/11f already found live — wire-shape mismatches and behavior that only
//! shows up when the processes actually talk over a socket — and an
//! in-process shortcut would just re-introduce the same blind spot unit
//! tests already have.

use std::io::ErrorKind;
use std::net::TcpListener as StdTcpListener;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, ensure, Context, Result};
use tokio::process::{Child, Command};
use tokio::time::Instant;

/// An OS-assigned loopback port, freed immediately before the caller binds
/// it again in a spawned child. Carries the usual tiny TOCTOU race of this
/// technique; acceptable here the same way `crates/gsp-core/tests/` already
/// leans on `127.0.0.1:0` for ephemeral sockets (CLAUDE.md's testing rule).
pub fn free_port() -> Result<u16> {
    let listener = StdTcpListener::bind("127.0.0.1:0").context("binding an ephemeral port")?;
    Ok(listener.local_addr()?.port())
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root must exist")
}

/// Debug-builds the four fleet binaries once (idempotent — `cargo build`
/// itself no-ops on an unchanged tree; callers don't need to coordinate
/// across tests, cargo's own target-dir lock serializes concurrent callers).
/// Debug, not release: this is a correctness test, not `gsp-bench`'s
/// latency harness, and a debug build is much faster to produce in CI.
pub fn build_fleet_bins() -> Result<()> {
    let status = std::process::Command::new("cargo")
        .args([
            "build",
            "-p",
            "gsp",
            "-p",
            "gsp-controller",
            "-p",
            "gsp-aggregator",
            "-p",
            "gsp-ui",
        ])
        .current_dir(workspace_root())
        .status()
        .context("running `cargo build -p gsp -p gsp-controller -p gsp-aggregator -p gsp-ui`")?;
    ensure!(status.success(), "building the fleet binaries failed");
    Ok(())
}

fn bin_path(name: &str) -> PathBuf {
    workspace_root().join("target/debug").join(name)
}

/// A spawned fleet process, killed on drop so a failing assertion (which
/// unwinds past every `Fleet*` in scope) never leaks a child that would
/// otherwise squat on its port for the rest of the test run.
pub struct Proc {
    name: &'static str,
    child: Child,
}

impl Drop for Proc {
    fn drop(&mut self) {
        if self.child.start_kill().is_err() {
            eprintln!("gsp-fleet-tests: {} was already gone on drop", self.name);
        }
    }
}

impl Proc {
    fn spawn(name: &'static str, args: &[String]) -> Result<Self> {
        let child = Command::new(bin_path(name))
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawning {name}"))?;
        Ok(Self { name, child })
    }

    /// Kill and reap the process now, instead of waiting for `Drop` — used
    /// by the "disconnect" half of a reconnect test, where the test needs to
    /// observe behavior *while* the process is known to be gone.
    pub async fn kill(mut self) -> Result<()> {
        self.child.start_kill().ok();
        let _ = self.child.wait().await;
        Ok(())
    }

    /// `Some(exit code)` if the process has already exited (non-blocking);
    /// `None` if it's still running.
    pub fn exit_code(&mut self) -> Option<i32> {
        self.child.try_wait().ok().flatten().and_then(|s| s.code())
    }
}

/// Poll `url` (a plain `GET`) until it returns any HTTP response or
/// `timeout` elapses. Every fleet binary here serves `GET /healthz`
/// unauthenticated, so this is the uniform "is it up" check.
pub async fn wait_http_up(url: &str, timeout: Duration) -> Result<()> {
    let client = reqwest::Client::new();
    let deadline = Instant::now() + timeout;
    loop {
        match client.get(url).send().await {
            Ok(_) => return Ok(()),
            Err(e) if Instant::now() < deadline => {
                if !(e.is_connect() || e.is_timeout()) {
                    bail!("unexpected error waiting for {url}: {e}");
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Err(e) => bail!("{url} never came up within {timeout:?}: {e}"),
        }
    }
}

/// Poll `f` until it returns `Ok(true)` or `timeout` elapses, returning the
/// last error/`false` reason as context. Used for state that isn't just
/// "process is up" (e.g. "instance X shows up in the aggregator's store").
pub async fn wait_until<F, Fut>(mut f: F, timeout: Duration, what: &str) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<bool>>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if f().await? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("timed out after {timeout:?} waiting for: {what}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A minimal, always-valid `gsp` config: one admin listener, one TCP
/// listener, one pool with a single (not necessarily reachable) target.
/// Reachability doesn't matter for these tests — nothing here pushes actual
/// game traffic through a pool, only exercises the control-plane surfaces
/// (controller pull, aggregator push, admin API, fan-out).
pub fn minimal_gsp_config(admin_port: u16, listen_port: u16, target_port: u16) -> String {
    format!(
        r#"
settings:
  workers: 1
  shutdown_grace_sec: 1
  admin:
    listen: "127.0.0.1:{admin_port}"
pools:
  - name: local
    targets: ["127.0.0.1:{target_port}"]
    balancer: round_robin
    health_check:
      type: tcp_connect
      interval_sec: 60
      timeout_ms: 200
      rise: 1
      fall: 1000
listeners:
  - name: tcp-in
    bind: "127.0.0.1:{listen_port}"
    protocol: tcp
    pool: local
"#
    )
}

/// A config that fails `gsp_config::validate()` on purpose (negative
/// `rise`), for the "bad submission never displaces the current revision"
/// test — this needs to be a *schema* rejection, not a YAML syntax error,
/// so it exercises the same `parse_str` path a good submission takes.
pub fn invalid_gsp_config() -> &'static str {
    r#"
settings:
  admin:
    listen: "127.0.0.1:9999"
pools:
  - name: local
    targets: ["127.0.0.1:1"]
    health_check:
      type: tcp_connect
      interval_sec: 2
      timeout_ms: 500
      rise: 0
      fall: 3
listeners:
  - name: tcp-in
    bind: "127.0.0.1:9998"
    protocol: tcp
    pool: local
"#
}

pub struct GspArgs {
    pub config_path: Option<PathBuf>,
    pub controller_url: Option<String>,
    pub controller_token: Option<String>,
    pub aggregator_url: Option<String>,
    pub aggregator_instance: Option<String>,
    pub aggregator_interval_sec: u64,
}

impl Default for GspArgs {
    fn default() -> Self {
        Self {
            config_path: None,
            controller_url: None,
            controller_token: None,
            aggregator_url: None,
            aggregator_instance: None,
            aggregator_interval_sec: 1,
        }
    }
}

pub fn spawn_gsp(args: GspArgs) -> Result<Proc> {
    let mut argv = Vec::new();
    if let Some(p) = &args.config_path {
        argv.push("--config".to_string());
        argv.push(p.display().to_string());
    }
    if let Some(u) = &args.controller_url {
        argv.push("--controller".to_string());
        argv.push(u.clone());
    }
    if let Some(t) = &args.controller_token {
        argv.push("--controller-token".to_string());
        argv.push(t.clone());
    }
    if let Some(u) = &args.aggregator_url {
        argv.push("--aggregator".to_string());
        argv.push(u.clone());
    }
    if let Some(n) = &args.aggregator_instance {
        argv.push("--aggregator-instance".to_string());
        argv.push(n.clone());
    }
    argv.push("--aggregator-interval-sec".to_string());
    argv.push(args.aggregator_interval_sec.to_string());
    Proc::spawn("gsp", &argv)
}

pub fn spawn_controller(data_dir: &Path, listen_port: u16) -> Result<Proc> {
    Proc::spawn(
        "gsp-controller",
        &[
            "--data-dir".to_string(),
            data_dir.display().to_string(),
            "--listen".to_string(),
            format!("127.0.0.1:{listen_port}"),
        ],
    )
}

pub fn spawn_aggregator(listen_port: u16) -> Result<Proc> {
    Proc::spawn(
        "gsp-aggregator",
        &["--listen".to_string(), format!("127.0.0.1:{listen_port}")],
    )
}

/// True once the port refuses new connections (used after killing a
/// process, to confirm the OS has actually reclaimed the socket before a
/// test tries something that depends on it staying down).
pub async fn port_is_down(port: u16, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        match tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
            Err(e) if e.kind() == ErrorKind::ConnectionRefused => return true,
            _ if Instant::now() >= deadline => return false,
            _ => tokio::time::sleep(Duration::from_millis(25)).await,
        }
    }
}
