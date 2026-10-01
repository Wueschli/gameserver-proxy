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
const ADMIN_PORT: u16 = 9900;
const CONTROLLER_PORT: u16 = 9901;
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

struct Origin {
    ns: Ns,
    agent: Proc,
    #[allow(dead_code)] // held for its Drop: stops the echo server
    echo: Option<EchoServer>,
    pubkey: String,
}

struct Edge {
    ns: Ns,
    gsp: Proc,
}

pub struct TunnelLab {
    pub backend: Backend,
    lab: Lab,
    controller: Proc,
    dir: tempfile::TempDir,
    origin: Option<Origin>,
    edges: Vec<Edge>,
}

impl TunnelLab {
    pub async fn new() -> Result<Self> {
        let lab = Lab::new()?;
        let dir = tempfile::tempdir()?;
        std::fs::create_dir_all(dir.path().join("controller"))?;
        let controller = spawn_controller_on(
            &dir.path().join("controller"),
            &format!("0.0.0.0:{CONTROLLER_PORT}"),
        )?;
        wait_http_up(
            &format!("http://127.0.0.1:{CONTROLLER_PORT}/healthz"),
            Duration::from_secs(10),
        )
        .await?;
        Ok(Self {
            backend: Backend::from_env(),
            lab,
            controller,
            dir,
            origin: None,
            edges: Vec::new(),
        })
    }

    fn controller_url(ns: &Ns) -> String {
        format!("http://{}:{CONTROLLER_PORT}", ns.lab_addr())
    }

    /// Start `gsp-agent` in a new namespace, wait until it has registered, and
    /// remember its pubkey. With `with_echo` the echo server starts too.
    pub async fn start_origin(&mut self, with_echo: bool) -> Result<()> {
        let ns = self.lab.add_ns()?;
        let mut args: Vec<String> = [
            "--data-dir",
            self.dir.path().join("agent").to_str().unwrap(),
            "--controller-url",
            &Self::controller_url(&ns),
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

        let url = format!("http://127.0.0.1:{CONTROLLER_PORT}/peers/{ORIGIN_NAME}");
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
            ns,
            agent,
            echo,
            pubkey,
        });
        Ok(())
    }

    pub fn start_echo(&mut self) -> Result<()> {
        let origin = self.origin.as_mut().context("start_origin first")?;
        origin.echo = Some(EchoServer::start(&origin.ns, ECHO_PORT)?);
        Ok(())
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
            &Self::controller_url(&ns),
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
        self.edges.push(Edge { ns, gsp });
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
        if !std::thread::panicking() {
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
