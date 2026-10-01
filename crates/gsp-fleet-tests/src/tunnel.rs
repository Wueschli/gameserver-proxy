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
use crate::{spawn_controller_on, wait_http_up, wait_until, Proc};

pub const ECHO_PORT: u16 = 7000;
pub const PUBLIC_PORT: u16 = 8000;
/// Not in any pool: a second echo port used only to prove the tunnel is up.
const PROBE_PORT: u16 = 7001;
const ADMIN_PORT: u16 = 9900;
const WG_PORT: u16 = 51820;
const ORIGIN_NAME: &str = "origin-a";
const ORIGIN_TUNNEL_IP: &str = "10.60.0.2";

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
pub fn edge_config(pubkey: &str) -> String {
    format!(
        r#"
settings:
  workers: 1
  shutdown_grace_sec: 1
  admin:
    listen: "0.0.0.0:{ADMIN_PORT}"
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
  - {{ name: tcp-in, bind: "0.0.0.0:{PUBLIC_PORT}", protocol: tcp, pool: tcp-pool }}
  - {{ name: udp-in, bind: "0.0.0.0:{PUBLIC_PORT}", protocol: udp, pool: udp-pool }}
"#
    )
}

// Field order is drop order: processes and the echo thread (which hold the
// namespace) go before the `Ns` itself.
struct Origin {
    agent: Proc,
    #[allow(dead_code)] // held for its Drop: stops the echo server
    echo: Option<EchoServer>,
    #[allow(dead_code)] // held for its Drop: stops the probe echo server
    probe: Option<EchoServer>,
    ns: Ns,
    pubkey: String,
}

struct Edge {
    gsp: Proc,
    ns: Ns,
}

pub struct TunnelLab {
    pub backend: Backend,
    /// Set by [`TunnelLab::pass`]; if the lab is dropped without it (a panic, or
    /// an `Err` returned with `?`) every process log is printed.
    passed: bool,
    controller_port: u16,
    // Drop order: scenario processes first, then the controller, then the rest.
    edges: Vec<Edge>,
    origin: Option<Origin>,
    controller: Proc,
    dir: tempfile::TempDir,
    lab: Lab,
}

impl TunnelLab {
    pub async fn new() -> Result<Self> {
        let lab = Lab::new()?;
        // A fresh port per lab: a previous test's controller may still be
        // closing its listener.
        let controller_port = crate::free_port()?;
        let dir = tempfile::tempdir()?;
        std::fs::create_dir_all(dir.path().join("controller"))?;
        let controller = spawn_controller_on(
            &dir.path().join("controller"),
            &format!("0.0.0.0:{controller_port}"),
        )?;
        wait_http_up(
            &format!("http://127.0.0.1:{controller_port}/healthz"),
            Duration::from_secs(10),
        )
        .await?;
        Ok(Self {
            backend: Backend::from_env(),
            passed: false,
            controller_port,
            edges: Vec::new(),
            origin: None,
            controller,
            dir,
            lab,
        })
    }

    fn controller_url(&self, ns: &Ns) -> String {
        format!("http://{}:{}", ns.lab_addr(), self.controller_port)
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
            "--address",
            &format!("{ORIGIN_TUNNEL_IP}/24"),
            "--backends",
            &format!("{ORIGIN_TUNNEL_IP}:{ECHO_PORT}"),
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

        let echo = if with_echo {
            Some(EchoServer::start(&ns, ECHO_PORT)?)
        } else {
            None
        };
        self.origin = Some(Origin {
            agent,
            echo,
            probe: None,
            ns,
            pubkey,
        });
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
        let ns = &self.edges[edge].ns;
        wait_until(
            || async move {
                ns.in_ns(|| {
                    use std::io::{Read, Write};
                    let addr = SocketAddr::new(ORIGIN_TUNNEL_IP.parse().unwrap(), PROBE_PORT);
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
            .is_some_and(|o| o.agent.exit_code().is_none())
    }

    /// Start `gsp --tunnel-*` in a new namespace. `pinned_pubkey` overrides the
    /// key written into the tunnel source (default: the origin's real one).
    /// Returns the edge's index for the other helpers.
    pub async fn start_edge(
        &mut self,
        name: &str,
        tunnel_last_octet: u8,
        pinned_pubkey: Option<&str>,
    ) -> Result<usize> {
        let pubkey = pinned_pubkey
            .unwrap_or_else(|| self.origin_pubkey())
            .to_string();
        let ns = self.lab.add_ns()?;
        let cfg: PathBuf = self.dir.path().join(format!("{name}.yaml"));
        std::fs::write(&cfg, edge_config(&pubkey))?;

        let mut args: Vec<String> = [
            "--config",
            cfg.to_str().unwrap(),
            "--tunnel-iface",
            "gsp-tunnel0",
            "--tunnel-listen-port",
            &WG_PORT.to_string(),
            "--tunnel-address",
            &format!("10.60.0.{tunnel_last_octet}/24"),
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
            &format!("{}:{WG_PORT}", ns.underlay()),
            "--tunnel-register-interval-sec",
            "1",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        args.extend(self.backend.gsp_flag().map(str::to_string));
        let gsp = Proc::spawn_in(Some(&ns), "gsp", &args)?;
        wait_http_up(
            &format!("http://{}:{ADMIN_PORT}/healthz", ns.underlay()),
            Duration::from_secs(20),
        )
        .await?;
        self.edges.push(Edge { gsp, ns });
        Ok(self.edges.len() - 1)
    }

    pub fn public_addr(&self, edge: usize) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(self.edges[edge].ns.underlay()), PUBLIC_PORT)
    }

    pub async fn pools(&self, edge: usize) -> Result<String> {
        let url = format!(
            "http://{}:{ADMIN_PORT}/pools",
            self.edges[edge].ns.underlay()
        );
        Ok(reqwest::get(url).await?.text().await?)
    }

    /// Wait until every backend line under every pool is (un)healthy, and there
    /// is at least one. `/pools` lines look like `  10.60.0.2:7000\thealthy\tstate=…`.
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
}

impl Drop for TunnelLab {
    fn drop(&mut self) {
        if self.passed {
            return;
        }
        eprintln!("\n=== tunnel e2e failed — process logs ===");
        let dump = |p: &Proc| eprintln!("--- {} ---\n{}", p.name(), p.log());
        dump(&self.controller);
        if let Some(o) = &self.origin {
            dump(&o.agent);
        }
        for e in &self.edges {
            dump(&e.gsp);
        }
    }
}
