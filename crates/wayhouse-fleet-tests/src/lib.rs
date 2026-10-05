//! Slice 12 (`docs/08-roadmap.md` phase 10+11): fleet integration tests.
//!
//! Every other phase 10+11 slice was verified by a human standing up real
//! `wayhouse` / `wayhouse-controller` / `wayhouse-aggregator` / `wayhouse-ui` processes and
//! driving them over real HTTP (see `HANDOVER.md`'s slice notes). This crate
//! automates exactly that shape of test instead of inventing a new
//! in-process harness: `tests/fleet.rs` spawns the real debug binaries as
//! child processes on `127.0.0.1` with OS-assigned ports, the same way a
//! human (or `wayhouse-bench`'s `concurrency` mode) would from a shell.
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

pub mod echo;
pub mod netns;
pub mod tls_front;
pub mod tunnel;

/// An OS-assigned loopback port, freed immediately before the caller binds
/// it again in a spawned child. Carries the usual tiny TOCTOU race of this
/// technique; acceptable here the same way `crates/wayhouse-core/tests/` already
/// leans on `127.0.0.1:0` for ephemeral sockets (AGENTS.md's testing rule).
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

/// Debug-builds the five fleet binaries once (idempotent — `cargo build`
/// itself no-ops on an unchanged tree; callers don't need to coordinate
/// across tests, cargo's own target-dir lock serializes concurrent callers).
/// Debug, not release: this is a correctness test, not `wayhouse-bench`'s
/// latency harness, and a debug build is much faster to produce in CI.
pub fn build_fleet_bins() -> Result<()> {
    build_bins(&[
        "wayhouse",
        "wayhouse-controller",
        "wayhouse-aggregator",
        "wayhouse-ui",
        "wayhouse-agent",
    ])
}

/// [`build_fleet_bins`] for the tunnel lab, which runs only these three. Also
/// what `make tunnel-e2e` pre-builds, so the build inside the namespace has
/// nothing left to compile.
pub fn build_tunnel_bins() -> Result<()> {
    build_bins(&["wayhouse", "wayhouse-controller", "wayhouse-agent"])
}

/// The `cargo build` that [`build_tunnel_bins`] runs, for a test that inspects it.
pub fn tunnel_bins_build_command() -> std::process::Command {
    build_command(&["wayhouse", "wayhouse-controller", "wayhouse-agent"])
}

fn build_bins(packages: &[&str]) -> Result<()> {
    let status = build_command(packages)
        .status()
        .with_context(|| format!("running `cargo build` for {packages:?}"))?;
    ensure!(status.success(), "building the fleet binaries failed");
    Ok(())
}

/// A `cargo build` that is a no-op on a tree already built from a shell.
/// `cargo test` sets `CARGO_PKG_*`, `CARGO_MANIFEST_*` and `CARGO_CRATE_NAME`
/// for the test binary; inherited by this nested cargo they differ from the
/// outer build's, so `ring`'s build script (it tracks them with
/// `rerun-if-env-changed`) reruns and `ring` recompiles on every call.
fn build_command(packages: &[&str]) -> std::process::Command {
    let mut cmd = std::process::Command::new("cargo");
    cmd.arg("build")
        .args(packages.iter().flat_map(|p| ["-p", p]))
        .current_dir(workspace_root());
    for (key, _) in std::env::vars_os() {
        let Some(key) = key.to_str() else { continue };
        if key.starts_with("CARGO_PKG_")
            || key.starts_with("CARGO_MANIFEST_")
            || key == "CARGO_CRATE_NAME"
        {
            cmd.env_remove(key);
        }
    }
    cmd
}

pub fn bin_path(name: &str) -> PathBuf {
    workspace_root().join("target/debug").join(name)
}

/// A spawned fleet process, killed on drop so a failing assertion (which
/// unwinds past every `Fleet*` in scope) never leaks a child that would
/// otherwise squat on its port for the rest of the test run.
pub struct Proc {
    name: &'static str,
    child: Child,
    /// Where this process's stdout+stderr go, if captured ([`Proc::spawn_in`]).
    log: Option<tempfile::TempPath>,
}

impl Drop for Proc {
    fn drop(&mut self) {
        if self.child.start_kill().is_err() {
            eprintln!(
                "wayhouse-fleet-tests: {} was already gone on drop",
                self.name
            );
            return;
        }
        // Reap before returning: the process may hold a network namespace (or a
        // port) the caller is about to reuse, and `start_kill` alone only sends
        // the signal.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                _ => return,
            }
        }
        eprintln!(
            "wayhouse-fleet-tests: {} did not exit within 5s of SIGKILL",
            self.name
        );
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
        Ok(Self {
            name,
            child,
            log: None,
        })
    }

    /// Like [`Proc::spawn`], but optionally inside a network namespace and with
    /// stdout+stderr captured to a temp file ([`Proc::log`]) so a failing
    /// scenario can print what every process said.
    pub fn spawn_in(
        ns: Option<&crate::netns::Ns>,
        name: &'static str,
        args: &[String],
    ) -> Result<Self> {
        let (file, path) = tempfile::NamedTempFile::new()?.into_parts();
        let file2 = file.try_clone()?;
        let mut cmd = match ns {
            // nsenter exec()s the target, so `child.id()` is the binary's pid.
            Some(ns) => {
                let mut c = Command::new("nsenter");
                c.arg(format!("--net={}", ns.ns_path()))
                    .arg("--")
                    .arg(bin_path(name));
                c
            }
            None => Command::new(bin_path(name)),
        };
        let child = cmd
            .args(args)
            .stdout(Stdio::from(file))
            .stderr(Stdio::from(file2))
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawning {name}"))?;
        Ok(Self {
            name,
            child,
            log: Some(path),
        })
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    /// Everything the process has written so far (empty if not captured).
    pub fn log(&self) -> String {
        self.log
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .unwrap_or_default()
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
/// `timeout` elapses, retrying every send error. Every fleet binary here
/// serves `GET /healthz` unauthenticated, so this is the uniform "is it up"
/// check.
pub async fn wait_http_up(url: &str, timeout: Duration) -> Result<()> {
    let client = reqwest::Client::new();
    let deadline = Instant::now() + timeout;
    loop {
        match client.get(url).send().await {
            Ok(_) => return Ok(()),
            // Any send error can be a startup race (refused, reset, or an
            // incomplete message while the binary binds), so keep retrying
            // until the deadline and report the last one.
            Err(_) if Instant::now() < deadline => {
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

/// A minimal, always-valid `wayhouse` config: one admin listener, one TCP
/// listener, one pool with a single (not necessarily reachable) target.
/// Reachability doesn't matter for these tests — nothing here pushes actual
/// game traffic through a pool, only exercises the control-plane surfaces
/// (controller pull, aggregator push, admin API, fan-out).
pub fn minimal_wayhouse_config(admin_port: u16, listen_port: u16, target_port: u16) -> String {
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

/// Like [`minimal_wayhouse_config`], plus `settings.failure_domain` /
/// `settings.gossip` (phase 13, docs/10 "Tier 2") and a caller-chosen
/// `fall` threshold — the one knob the gossip fleet tests vary per instance
/// to create a real, live "this instance's own local view differs from the
/// domain's" scenario without needing to fake any wire data: every instance
/// still runs a real active health check against the same real (unreachable)
/// target, but a high `fall` means *this* instance's own `Backend::observe`
/// never flips its local `healthy` flag, even though it still (truthfully)
/// publishes its raw per-check result into the mesh like everyone else.
#[allow(clippy::too_many_arguments)]
pub fn gossip_wayhouse_config(
    admin_port: u16,
    listen_port: u16,
    target_port: u16,
    gossip_port: u16,
    seed_gossip_port: Option<u16>,
    fall: u32,
    quorum_fraction: f64,
    failure_domain: &str,
    psk: &str,
) -> String {
    let seeds = match seed_gossip_port {
        Some(p) => format!("[\"127.0.0.1:{p}\"]"),
        None => "[]".to_string(),
    };
    format!(
        r#"
settings:
  workers: 1
  shutdown_grace_sec: 1
  admin:
    listen: "127.0.0.1:{admin_port}"
  failure_domain: "{failure_domain}"
  gossip:
    bind: "127.0.0.1:{gossip_port}"
    seeds: {seeds}
    quorum_fraction: {quorum_fraction}
    psk: "{psk}"
pools:
  - name: local
    targets: ["127.0.0.1:{target_port}"]
    balancer: round_robin
    health_check:
      type: tcp_connect
      interval_sec: 1
      timeout_ms: 200
      rise: 1
      fall: {fall}
listeners:
  - name: tcp-in
    bind: "127.0.0.1:{listen_port}"
    protocol: tcp
    pool: local
"#
    )
}

/// A config that fails `wayhouse_config::validate()` on purpose (negative
/// `rise`), for the "bad submission never displaces the current revision"
/// test — this needs to be a *schema* rejection, not a YAML syntax error,
/// so it exercises the same `parse_str` path a good submission takes.
pub fn invalid_wayhouse_config() -> &'static str {
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

pub struct WayhouseArgs {
    pub config_path: Option<PathBuf>,
    pub controller_url: Option<String>,
    pub controller_token: Option<String>,
    pub aggregator_url: Option<String>,
    pub aggregator_instance: Option<String>,
    pub aggregator_interval_sec: u64,
    /// Further command-line arguments, appended last (e.g. `--ca-file`).
    pub extra: Vec<String>,
}

impl Default for WayhouseArgs {
    fn default() -> Self {
        Self {
            config_path: None,
            controller_url: None,
            controller_token: None,
            aggregator_url: None,
            aggregator_instance: None,
            aggregator_interval_sec: 1,
            extra: Vec::new(),
        }
    }
}

pub fn spawn_wayhouse(args: WayhouseArgs) -> Result<Proc> {
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
    argv.extend(args.extra);
    Proc::spawn("wayhouse", &argv)
}

pub fn spawn_controller(data_dir: &Path, listen_port: u16) -> Result<Proc> {
    Proc::spawn(
        "wayhouse-controller",
        &[
            "--data-dir".to_string(),
            data_dir.display().to_string(),
            "--listen".to_string(),
            format!("127.0.0.1:{listen_port}"),
        ],
    )
}

/// Controller on an arbitrary `host:port` (the tunnel e2e test binds
/// `0.0.0.0` so every namespace can reach it), output captured.
pub fn spawn_controller_on(data_dir: &Path, listen: &str) -> Result<Proc> {
    spawn_controller_with(data_dir, listen, &[])
}

/// Like [`spawn_controller_on`] with extra command-line arguments.
pub fn spawn_controller_with(data_dir: &Path, listen: &str, extra: &[String]) -> Result<Proc> {
    let mut args = vec![
        "--data-dir".to_string(),
        data_dir.display().to_string(),
        "--listen".to_string(),
        listen.to_string(),
    ];
    args.extend(extra.iter().cloned());
    // HA refuses to start without a peer token: give those tests a shared one.
    let ha = args.iter().any(|a| a == "--ha-peers" || a == "--ha-join");
    if ha && !args.iter().any(|a| a == "--ha-token") {
        args.extend(["--ha-token".to_string(), "fleet-test-ha-token".to_string()]);
    }
    Proc::spawn_in(None, "wayhouse-controller", &args)
}

pub fn spawn_aggregator(listen_port: u16) -> Result<Proc> {
    spawn_aggregator_with(listen_port, &[])
}

/// Like [`spawn_aggregator`] with extra command-line arguments.
pub fn spawn_aggregator_with(listen_port: u16, extra: &[String]) -> Result<Proc> {
    let mut args = vec!["--listen".to_string(), format!("127.0.0.1:{listen_port}")];
    args.extend(extra.iter().cloned());
    Proc::spawn("wayhouse-aggregator", &args)
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

/// `wayhouse-ui` on `127.0.0.1:listen_port` with extra command-line arguments.
pub fn spawn_ui_with(listen_port: u16, extra: &[String]) -> Result<Proc> {
    let mut args = vec!["--listen".to_string(), format!("127.0.0.1:{listen_port}")];
    args.extend(extra.iter().cloned());
    Proc::spawn("wayhouse-ui", &args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    /// A binary that is still starting can reset or cut off a request; that
    /// is not a connect error, but `wait_http_up` must keep retrying it.
    #[tokio::test]
    async fn wait_http_up_retries_a_connection_cut_off_mid_request() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/healthz", listener.local_addr().unwrap());
        tokio::spawn(async move {
            // First request: hang up without answering (an incomplete message).
            drop(listener.accept().await.unwrap());
            loop {
                let (mut s, _) = listener.accept().await.unwrap();
                let _ = s
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n")
                    .await;
            }
        });
        wait_http_up(&url, Duration::from_secs(5)).await.unwrap();
    }
}
