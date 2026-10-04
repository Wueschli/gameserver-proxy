//! Bring-up and helpers for the phase-14 tunnel e2e scenarios — see
//! `docs/superpowers/specs/2026-10-01-tunnel-e2e-design.md` ("Bring-up order").
//! The origin must register before `gsp` starts because `gsp`'s tunnel
//! `backend_sources[].pubkey` pins the origin's key; we read it back from the
//! controller's backend-peers registry.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::echo::{tcp_roundtrip, EchoServer};
use crate::netns::{Lab, Ns};
use crate::{port_is_down, spawn_controller_with, wait_http_up, wait_until, Proc};

pub const ECHO_PORT: u16 = 7000;
pub const PUBLIC_PORT: u16 = 8000;
/// Not in any pool: a second echo port used only to prove the tunnel is up.
const PROBE_PORT: u16 = 7001;
const ADMIN_PORT: u16 = 9900;
const WG_PORT: u16 = 51820;
const ORIGIN_NAME: &str = "origin-a";

/// Which family the lab's underlay (WireGuard endpoints, controller URL,
/// public listeners) uses. Independent of the tunnel network's family.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Underlay {
    V4,
    V6,
}

#[derive(Clone, Copy, Debug)]
pub struct LabOptions {
    /// The controller's `--tunnel-network`.
    pub tunnel_network: &'static str,
    pub underlay: Underlay,
}

impl Default for LabOptions {
    /// IPv6 tunnel network (the documented default) over an IPv4 underlay.
    fn default() -> Self {
        LabOptions {
            tunnel_network: "fd49:89c1:4b5e:60::/64",
            underlay: Underlay::V4,
        }
    }
}

/// `--tunnel-network <network>` followed by `extra`, unless `extra` names its
/// own network.
fn controller_args(network: &str, extra: &[&str]) -> Vec<String> {
    // The lab controller binds the wildcard address and the lab agents carry no token.
    let mut args = vec!["--insecure-no-auth".to_string()];
    if !extra.contains(&"--tunnel-network") {
        args.extend(["--tunnel-network".to_string(), network.to_string()]);
    }
    args.extend(extra.iter().map(|s| s.to_string()));
    args
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Backend {
    Kernel,
    Userspace,
}

impl Backend {
    /// `TUNNEL_BACKEND=kernel|userspace`, default `kernel`.
    pub fn from_env() -> Self {
        match std::env::var("TUNNEL_BACKEND").as_deref() {
            Ok("userspace") => Backend::Userspace,
            Ok("kernel") | Err(_) => Backend::Kernel,
            Ok(other) => panic!("TUNNEL_BACKEND must be `kernel` or `userspace`, got {other:?}"),
        }
    }

    /// Per-wait deadline. Spike (2026-10-01): kernel's first round trip is ~2 s;
    /// userspace's was ~25 s (waits on the agent's persistent keepalive).
    pub fn deadline(self) -> Duration {
        match self {
            Backend::Kernel => Duration::from_secs(30),
            Backend::Userspace => Duration::from_secs(90),
        }
    }

    fn agent_flag(self) -> Option<&'static str> {
        (self == Backend::Userspace).then_some("--userspace")
    }

    fn gsp_flag(self) -> Option<&'static str> {
        (self == Backend::Userspace).then_some("--tunnel-userspace")
    }
}

/// An edge `gsp` config: one `tunnel` source pinned to `pubkey`, a TCP pool
/// and a UDP pool both fed by it, and a TCP + UDP listener on `:8000`.
/// `any` is the wildcard host the edge binds: `0.0.0.0` or `[::]`.
pub fn edge_config(pubkey: &str, any: &str) -> String {
    format!(
        r#"
settings:
  workers: 1
  shutdown_grace_sec: 1
  admin:
    listen: "{any}:{ADMIN_PORT}"
backend_sources:
  - name: {ORIGIN_NAME}
    type: tunnel
    pubkey: "{pubkey}"
    refresh_interval_sec: 1
pools:
  - name: tcp-pool
    source: {ORIGIN_NAME}
    health_check: {{ type: tcp_connect, interval_sec: 1, timeout_ms: 500, rise: 1, fall: 2 }}
  - name: udp-pool
    source: {ORIGIN_NAME}
    health_check: {{ type: udp_probe, send_hex: "aa", expect_hex_prefix: "aa", interval_sec: 1, timeout_ms: 500, rise: 1, fall: 2 }}
listeners:
  - {{ name: tcp-in, bind: "{any}:{PUBLIC_PORT}", protocol: tcp, pool: tcp-pool }}
  - {{ name: udp-in, bind: "{any}:{PUBLIC_PORT}", protocol: udp, pool: udp-pool }}
"#
    )
}

// Field order is drop order: processes and the echo thread (which hold the
// namespace) go before the `Ns` itself.
struct Origin {
    /// `None` only transiently, while [`TunnelLab::restart_origin`] swaps it.
    agent: Option<Proc>,
    #[allow(dead_code)] // held for its Drop: stops the echo server
    echo: Option<EchoServer>,
    #[allow(dead_code)] // held for its Drop: stops the probe echo server
    probe: Option<EchoServer>,
    ns: Ns,
    args: Vec<String>,
    pubkey: String,
    /// The tunnel address the controller allocated to this origin.
    ip: IpAddr,
}

struct Edge {
    /// `None` only transiently, while [`TunnelLab::restart_edge`] swaps it.
    gsp: Option<Proc>,
    ns: Ns,
    args: Vec<String>,
}

pub struct TunnelLab {
    pub backend: Backend,
    opts: LabOptions,
    /// The controller's current arguments (see [`TunnelLab::restart_controller`]).
    controller_args: Vec<String>,
    /// Set by [`TunnelLab::pass`]; if the lab is dropped without it (a panic, or
    /// an `Err` returned with `?`) every process log is printed.
    passed: bool,
    controller_port: u16,
    // Drop order: scenario processes first, then the controller, then the rest.
    edges: Vec<Edge>,
    origin: Option<Origin>,
    controller: Option<Proc>,
    dir: tempfile::TempDir,
    lab: Lab,
}

impl TunnelLab {
    pub async fn new() -> Result<Self> {
        Self::with(LabOptions::default()).await
    }

    pub async fn with(opts: LabOptions) -> Result<Self> {
        let lab = Lab::new()?;
        // A fresh port per lab: a previous test's controller may still be
        // closing its listener.
        let controller_port = crate::free_port()?;
        let dir = tempfile::tempdir()?;
        std::fs::create_dir_all(dir.path().join("controller"))?;
        let controller_args = controller_args(opts.tunnel_network, &[]);
        let controller = spawn_controller_with(
            &dir.path().join("controller"),
            &controller_listen(opts.underlay, controller_port),
            &controller_args,
        )?;
        wait_http_up(
            &format!("http://127.0.0.1:{controller_port}/healthz"),
            Duration::from_secs(10),
        )
        .await?;
        Ok(Self {
            backend: Backend::from_env(),
            opts,
            controller_args,
            passed: false,
            controller_port,
            edges: Vec::new(),
            origin: None,
            controller: Some(controller),
            dir,
            lab,
        })
    }

    /// `ns`'s own underlay address in the lab's underlay family.
    fn underlay(&self, ns: &Ns) -> IpAddr {
        match self.opts.underlay {
            Underlay::V4 => IpAddr::V4(ns.underlay()),
            Underlay::V6 => IpAddr::V6(ns.underlay6()),
        }
    }

    /// The wildcard host an edge binds in this underlay family.
    fn any(&self) -> &'static str {
        match self.opts.underlay {
            Underlay::V4 => "0.0.0.0",
            Underlay::V6 => "[::]",
        }
    }

    fn controller_url(&self, ns: &Ns) -> String {
        let lab = match self.opts.underlay {
            Underlay::V4 => IpAddr::V4(ns.lab_addr()),
            Underlay::V6 => IpAddr::V6(ns.lab_addr6()),
        };
        format!("http://{}", SocketAddr::new(lab, self.controller_port))
    }

    fn admin_url(&self, edge: usize, path: &str) -> String {
        let addr = SocketAddr::new(self.underlay(&self.edges[edge].ns), ADMIN_PORT);
        format!("http://{addr}{path}")
    }

    /// The tunnel network's prefix length, e.g. `64`.
    pub fn tunnel_prefix(&self) -> &'static str {
        self.opts
            .tunnel_network
            .rsplit_once('/')
            .map(|(_, p)| p)
            .expect("a tunnel network is ip/prefix")
    }

    /// Mark the scenario as successful so `Drop` stays quiet.
    pub fn pass(&mut self) {
        self.passed = true;
    }

    /// Start `gsp-agent` in a new namespace, wait until it has registered, and
    /// remember its pubkey. With `with_echo` the echo server starts too.
    pub async fn start_origin(&mut self, with_echo: bool) -> Result<()> {
        let ns = self.lab.add_ns()?;
        let mut args: Vec<String> = [
            "--data-dir",
            self.dir.path().join("agent").to_str().unwrap(),
            "--controller-url",
            &self.controller_url(&ns),
            "--name",
            ORIGIN_NAME,
            "--iface",
            "gsp-agent0",
            "--listen-port",
            &WG_PORT.to_string(),
            "--backends",
            &format!(":{ECHO_PORT}"),
            "--register-interval-sec",
            "1",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        args.extend(self.backend.agent_flag().map(str::to_string));
        let agent = Proc::spawn_in(Some(&ns), "gsp-agent", &args)?;

        let url = format!(
            "http://127.0.0.1:{}/peers/{ORIGIN_NAME}",
            self.controller_port
        );
        wait_until(
            || {
                let url = url.clone();
                async move { Ok(reqwest::get(&url).await?.status().is_success()) }
            },
            Duration::from_secs(20),
            "the origin to register with the controller",
        )
        .await?;
        let body: serde_json::Value = reqwest::get(&url).await?.json().await?;
        let pubkey = body["pubkey"]
            .as_str()
            .context("registry entry has no pubkey")?
            .to_string();
        let ip: IpAddr = body["tunnel_address"]
            .as_str()
            .context("registry entry has no tunnel_address")?
            .parse()?;

        let echo = if with_echo {
            Some(EchoServer::start(&ns, ECHO_PORT)?)
        } else {
            None
        };
        self.origin = Some(Origin {
            agent: Some(agent),
            echo,
            probe: None,
            ns,
            args,
            pubkey,
            ip,
        });
        Ok(())
    }

    /// Kill the origin's agent and start it again with the same arguments in
    /// the same namespace (its echo servers keep running), then wait until it
    /// has registered and record its (possibly new) tunnel address. A killed
    /// agent leaves its interface (or boringtun socket) behind, so both are
    /// removed first, as in [`TunnelLab::restart_edge`].
    pub async fn restart_origin(&mut self) -> Result<()> {
        let url = self.registry_url(&format!("/peers/{ORIGIN_NAME}"));
        let before = reqwest::get(&url)
            .await?
            .json::<serde_json::Value>()
            .await?["tunnel_address"]
            .clone();
        let origin = self.origin.as_mut().context("start_origin first")?;
        origin.agent = None;
        tokio::time::sleep(Duration::from_millis(500)).await;
        let _ = origin.ns.run(&["ip", "link", "del", "gsp-agent0"]);
        let _ = std::fs::remove_file("/run/wireguard/gsp-agent0.sock");
        origin.agent = Some(Proc::spawn_in(Some(&origin.ns), "gsp-agent", &origin.args)?);
        // Re-registration is idempotent, so wait for the agent's own log
        // line rather than the registry entry (which already exists).
        let deadline = std::time::Instant::now() + Duration::from_secs(40);
        loop {
            let log = self
                .origin
                .as_ref()
                .and_then(|o| o.agent.as_ref())
                .map(Proc::log);
            let log = log.unwrap_or_default();
            if log.contains("tunnel address ready") {
                break;
            }
            anyhow::ensure!(
                std::time::Instant::now() < deadline,
                "the restarted origin never brought up its tunnel address (was {before})"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let body: serde_json::Value = reqwest::get(&url).await?.json().await?;
        let ip: IpAddr = body["tunnel_address"]
            .as_str()
            .context("registry entry has no tunnel_address")?
            .parse()?;
        self.origin.as_mut().expect("set above").ip = ip;
        Ok(())
    }

    pub fn start_echo(&mut self) -> Result<()> {
        let origin = self.origin.as_mut().context("start_origin first")?;
        origin.echo = Some(EchoServer::start(&origin.ns, ECHO_PORT)?);
        Ok(())
    }

    /// Prove the WireGuard tunnel itself is up, independent of any pool or of
    /// whether the pooled backend (`:7000`) is listening: start a probe echo on
    /// a second port in the origin namespace and connect to it *from the edge
    /// namespace* over the tunnel address. Lets a scenario then attribute an
    /// unhealthy `:7000` to `:7000` alone, not to a handshake still pending
    /// (userspace's first handshake takes ~25 s).
    pub async fn wait_tunnel_up(&mut self, edge: usize) -> Result<()> {
        let origin = self.origin.as_mut().context("start_origin first")?;
        if origin.probe.is_none() {
            origin.probe = Some(EchoServer::start(&origin.ns, PROBE_PORT)?);
        }
        let origin_ip = origin.ip;
        let ns = &self.edges[edge].ns;
        wait_until(
            || async move {
                ns.in_ns(move || {
                    use std::io::{Read, Write};
                    let addr = SocketAddr::new(origin_ip, PROBE_PORT);
                    let Ok(mut s) =
                        std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(1))
                    else {
                        return false;
                    };
                    let _ = s.set_read_timeout(Some(Duration::from_secs(1)));
                    let mut buf = [0u8; 4];
                    s.write_all(b"ping").is_ok()
                        && s.read_exact(&mut buf).is_ok()
                        && &buf == b"ping"
                })
            },
            self.backend.deadline(),
            "the WireGuard tunnel to carry traffic from the edge to the origin",
        )
        .await
    }

    pub fn origin_pubkey(&self) -> &str {
        &self.origin.as_ref().expect("start_origin first").pubkey
    }

    pub fn agent_alive(&mut self) -> bool {
        self.origin
            .as_mut()
            .and_then(|o| o.agent.as_mut())
            .is_some_and(|a| a.exit_code().is_none())
    }

    /// Start `gsp --tunnel-*` in a new namespace. `pinned_pubkey` overrides the
    /// key written into the tunnel source (default: the origin's real one).
    /// Returns the edge's index for the other helpers.
    pub async fn start_edge(&mut self, name: &str, pinned_pubkey: Option<&str>) -> Result<usize> {
        let pubkey = pinned_pubkey
            .unwrap_or_else(|| self.origin_pubkey())
            .to_string();
        let ns = self.lab.add_ns()?;
        let cfg: PathBuf = self.dir.path().join(format!("{name}.yaml"));
        std::fs::write(&cfg, edge_config(&pubkey, self.any()))?;

        let mut args: Vec<String> = [
            "--config",
            cfg.to_str().unwrap(),
            "--tunnel-iface",
            "gsp-tunnel0",
            "--tunnel-listen-port",
            &WG_PORT.to_string(),
            "--tunnel-key-file",
            self.dir
                .path()
                .join(format!("{name}.key"))
                .to_str()
                .unwrap(),
            "--tunnel-controller-url",
            &self.controller_url(&ns),
            "--tunnel-name",
            name,
            "--tunnel-endpoint",
            &SocketAddr::new(self.underlay(&ns), WG_PORT).to_string(),
            "--tunnel-register-interval-sec",
            "1",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        args.extend(self.backend.gsp_flag().map(str::to_string));
        // The lab edge's admin API binds the wildcard address with no token.
        args.push("--insecure-no-auth".to_string());
        let gsp = Proc::spawn_in(Some(&ns), "gsp", &args)?;
        self.edges.push(Edge {
            gsp: Some(gsp),
            ns,
            args,
        });
        let idx = self.edges.len() - 1;
        wait_http_up(&self.admin_url(idx, "/healthz"), Duration::from_secs(20)).await?;
        Ok(idx)
    }

    pub fn public_addr(&self, edge: usize) -> SocketAddr {
        SocketAddr::new(self.underlay(&self.edges[edge].ns), PUBLIC_PORT)
    }

    pub async fn pools(&self, edge: usize) -> Result<String> {
        Ok(reqwest::get(self.admin_url(edge, "/pools"))
            .await?
            .text()
            .await?)
    }

    /// Wait until every backend line under every pool is (un)healthy, and there
    /// is at least one. `/pools` lines look like `  [fd49::1]:7000\thealthy\tstate=…`.
    pub async fn wait_backends(&self, edge: usize, healthy: bool) -> Result<()> {
        let want = if healthy {
            "\thealthy\t"
        } else {
            "\tunhealthy\t"
        };
        wait_until(
            || async move {
                let text = self.pools(edge).await?;
                let lines: Vec<&str> = text.lines().filter(|l| l.starts_with("  ")).collect();
                Ok(!lines.is_empty() && lines.iter().all(|l| l.contains(want)))
            },
            self.backend.deadline(),
            if healthy {
                "all tunnel backends to become healthy"
            } else {
                "all tunnel backends to become unhealthy"
            },
        )
        .await
    }

    /// Poll a real TCP round trip through the public listener until it works.
    /// This — not `/pools` — proves the tunnel is up (health starts optimistic).
    pub async fn wait_roundtrip(&self, edge: usize) -> Result<()> {
        let addr = self.public_addr(edge);
        wait_until(
            || async move { Ok(tcp_roundtrip(addr, b"ping").await.is_ok()) },
            self.backend.deadline(),
            "a TCP round trip through the tunnel",
        )
        .await
    }
    pub fn origin_ip(&self) -> IpAddr {
        self.origin.as_ref().expect("start_origin first").ip
    }

    fn registry_url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.controller_port)
    }

    /// Run a command in an edge's namespace, returning its stdout.
    pub fn edge_run(&self, edge: usize, argv: &[&str]) -> Result<String> {
        self.edges[edge].ns.run(argv)
    }

    /// A proxy's stored registration, as `GET /proxy-peers/{name}` returns it.
    pub async fn proxy_registration(&self, name: &str) -> Result<serde_json::Value> {
        Ok(
            reqwest::get(self.registry_url(&format!("/proxy-peers/{name}")))
                .await?
                .json()
                .await?,
        )
    }

    /// The tunnel address the controller assigned to a registered proxy.
    pub async fn proxy_address(&self, name: &str) -> Result<String> {
        let body: serde_json::Value =
            reqwest::get(self.registry_url(&format!("/proxy-peers/{name}")))
                .await?
                .json()
                .await?;
        Ok(body["tunnel_address"]
            .as_str()
            .context("no tunnel_address in the proxy's registration")?
            .to_string())
    }

    /// When the controller last heard from a proxy (unix seconds), from the
    /// address table.
    pub async fn proxy_last_seen(&self, name: &str) -> Result<u64> {
        let table: serde_json::Value = reqwest::get(self.registry_url("/tunnel/addresses"))
            .await?
            .json()
            .await?;
        table["entries"]
            .as_array()
            .context("no entries in the address table")?
            .iter()
            .find(|e| e["role"] == "proxy" && e["name"] == name)
            .and_then(|e| e["last_seen"].as_u64())
            .with_context(|| format!("proxy {name:?} is not in the address table"))
    }

    /// Kill the controller and wait until its port refuses connections.
    pub async fn stop_controller(&mut self) -> Result<()> {
        self.controller = None;
        anyhow::ensure!(
            port_is_down(self.controller_port, Duration::from_secs(10)).await,
            "the controller's port did not close"
        );
        Ok(())
    }

    /// Start the controller again on the same port and data directory. `sled`
    /// releases its file lock on a background thread after a kill, so the first
    /// attempts can fail: retry.
    pub async fn start_controller(&mut self) -> Result<()> {
        let data = self.dir.path().join("controller");
        let listen = controller_listen(self.opts.underlay, self.controller_port);
        let mut last = None;
        for _ in 0..20 {
            let ctl = spawn_controller_with(&data, &listen, &self.controller_args)?;
            match wait_http_up(&self.registry_url("/healthz"), Duration::from_secs(3)).await {
                Ok(()) => {
                    self.controller = Some(ctl);
                    return Ok(());
                }
                Err(e) => {
                    last = Some(e);
                    drop(ctl);
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
            }
        }
        Err(last.expect("at least one attempt ran")).context("restarting the controller")
    }

    /// Stop the controller and start it again with `--tunnel-network <lab
    /// network>` followed by `extra` (or `extra` alone if it names a network).
    pub async fn restart_controller(&mut self, extra: &[&str]) -> Result<()> {
        self.stop_controller().await?;
        self.controller_args = controller_args(self.opts.tunnel_network, extra);
        self.start_controller().await
    }

    /// Stop the controller, start it with `args` (as for
    /// [`TunnelLab::restart_controller`]) and expect it to refuse to start:
    /// returns its log once it has exited non-zero. The controller stays down.
    pub async fn controller_refused(&mut self, args: &[&str]) -> Result<String> {
        if self.controller.is_some() {
            self.stop_controller().await?;
        }
        let data = self.dir.path().join("controller");
        let listen = controller_listen(self.opts.underlay, self.controller_port);
        let args = controller_args(self.opts.tunnel_network, args);
        let mut log = String::new();
        // sled may still hold the previous process's lock for a moment; that
        // exit is not the refusal under test, so try again.
        for _ in 0..20 {
            let mut ctl = spawn_controller_with(&data, &listen, &args)?;
            wait_until(
                || {
                    let code = ctl.exit_code();
                    async move { Ok(code.is_some_and(|c| c != 0)) }
                },
                Duration::from_secs(10),
                "the controller to refuse to start",
            )
            .await?;
            log = ctl.log();
            if log.contains("--tunnel-network") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        Ok(log)
    }

    /// Kill an edge's `gsp` and start it again with the same arguments in the
    /// same namespace. A killed kernel-backend `gsp` leaves its WireGuard
    /// interface behind (and a killed boringtun its socket), so both are
    /// removed first — what a supervisor or an operator would do.
    ///
    /// Waits up to 45 s for `/healthz`: with the controller down, `gsp` spends
    /// its ~30 s startup registration budget before falling back to the saved
    /// address and binding its listeners.
    pub async fn restart_edge(&mut self, idx: usize) -> Result<()> {
        self.edges[idx].gsp = None;
        tokio::time::sleep(Duration::from_millis(500)).await;
        let _ = self.edges[idx]
            .ns
            .run(&["ip", "link", "del", "gsp-tunnel0"]);
        let _ = std::fs::remove_file("/run/wireguard/gsp-tunnel0.sock");
        let gsp = Proc::spawn_in(Some(&self.edges[idx].ns), "gsp", &self.edges[idx].args)?;
        self.edges[idx].gsp = Some(gsp);
        wait_http_up(&self.admin_url(idx, "/healthz"), Duration::from_secs(45)).await
    }

    /// Start a second agent that pins `address_cidr` and expect the controller
    /// to refuse it: returns the agent's log once it has exited non-zero.
    pub async fn agent_refused(&mut self, name: &str, address_cidr: &str) -> Result<String> {
        let ns = self.lab.add_ns()?;
        let data = self.dir.path().join(format!("agent-{name}"));
        let mut args: Vec<String> = [
            "--data-dir",
            data.to_str().unwrap(),
            "--controller-url",
            &self.controller_url(&ns),
            "--name",
            name,
            "--iface",
            "gsp-agent1",
            "--listen-port",
            &WG_PORT.to_string(),
            "--address",
            address_cidr,
            "--backends",
            &format!(":{ECHO_PORT}"),
            "--register-interval-sec",
            "1",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        args.extend(self.backend.agent_flag().map(str::to_string));
        let mut agent = Proc::spawn_in(Some(&ns), "gsp-agent", &args)?;
        wait_until(
            || {
                let code = agent.exit_code();
                async move { Ok(code.is_some_and(|c| c != 0)) }
            },
            Duration::from_secs(40),
            "the refused agent to exit non-zero",
        )
        .await?;
        Ok(agent.log())
    }
}

/// The controller listens on every address of the underlay family. `[::]`
/// is dual stack, so the lab's own `127.0.0.1` checks still reach it.
fn controller_listen(underlay: Underlay, port: u16) -> String {
    match underlay {
        Underlay::V4 => format!("0.0.0.0:{port}"),
        Underlay::V6 => format!("[::]:{port}"),
    }
}

impl Drop for TunnelLab {
    fn drop(&mut self) {
        if self.passed {
            return;
        }
        eprintln!("\n=== tunnel e2e failed — process logs ===");
        let dump = |p: &Proc| eprintln!("--- {} ---\n{}", p.name(), p.log());
        if let Some(c) = &self.controller {
            dump(c);
        }
        if let Some(a) = self.origin.as_ref().and_then(|o| o.agent.as_ref()) {
            dump(a);
        }
        for e in &self.edges {
            if let Some(g) = &e.gsp {
                dump(g);
            }
        }
    }
}
