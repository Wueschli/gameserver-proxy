//! `gsp-agent` — the origin-side agent for phase 14 backend transport
//! (`docs/11-backend-transport.md`, `docs/08` "Phase 14" slice 3). Runs on a
//! host behind a game server's network boundary (a home NAT, a different
//! datacenter — anywhere `docs/01`'s "trusted internal network" assumption
//! no longer holds): brings up one local WireGuard interface via
//! `defguard/wireguard-rs` ([`interface`]) and registers its pubkey +
//! fronted backend addresses with `gsp-controller`'s backend-peers registry
//! ([`register`], phase 14 slice 2), keyed by a persisted identity
//! ([`keypair`]) so re-registering never changes this origin's pubkey.
//!
//! **Peering with the edge proxy**: `gsp`'s side (phase 14 slice 4) only
//! adds *this* agent as a peer once it sees the registration — it never
//! tells this agent about itself. So this agent also takes `--peer-pubkey`/
//! `--peer-endpoint` for the proxy's own identity, statically pinned (a
//! fleet of proxies per origin isn't a locked design yet, `docs/11`), and
//! configures it as a peer with a keepalive so the tunnel actually forms
//! end to end instead of each side only ever seeing half a picture.

mod interface;
mod keypair;
mod register;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use defguard_wireguard_rs::key::Key;
use defguard_wireguard_rs::net::IpAddrMask;
use defguard_wireguard_rs::peer::Peer;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(
    name = "gsp-agent",
    version,
    about = "Origin-side WireGuard agent for gsp's backend transport (phase 14, docs/11)"
)]
struct Args {
    /// Directory holding this agent's persisted WireGuard private key
    /// (created if missing). The key is this origin's stable identity —
    /// never regenerated once it exists.
    #[arg(long, default_value = "gsp-agent-data")]
    data_dir: PathBuf,

    /// Base URL of the `gsp-controller` backend-peers registry, e.g.
    /// `http://127.0.0.1:9901`.
    #[arg(long)]
    controller_url: String,

    /// Bearer token for the controller, if `--auth-token` is set there.
    #[arg(long)]
    controller_token: Option<String>,

    /// This origin's stable identity — what a pool's `backend_sources[].
    /// pubkey`-pinned `tunnel` source (phase 14 slice 5) will look this
    /// origin up by.
    #[arg(long)]
    name: String,

    /// Local WireGuard interface name.
    #[arg(long, default_value = "gsp-agent0")]
    iface: String,

    /// UDP port this interface listens on for the tunnel.
    #[arg(long, default_value_t = 51820)]
    listen_port: u16,

    /// This interface's own tunnel-internal address, `ip/cidr` (e.g.
    /// `10.60.0.2/24`).
    #[arg(long)]
    address: String,

    /// Backend addresses (tunnel-internal `ip:port`, reachable once this
    /// interface is up) this origin fronts.
    #[arg(long, value_delimiter = ',')]
    backends: Vec<String>,

    /// This origin's last-known public endpoint to advertise to the
    /// controller — informational only, since WireGuard's own roaming +
    /// keepalive mean it never has to stay correct after the first
    /// handshake (`docs/11` "Locked decisions"). Omit if this origin has no
    /// stable public address at all (e.g. behind a home NAT).
    #[arg(long)]
    endpoint: Option<String>,

    /// How often to re-register with the controller.
    #[arg(long, default_value_t = 30)]
    register_interval_sec: u64,

    /// Skip the kernel WireGuard backend and use boringtun userspace
    /// directly — for hosts with no `CAP_NET_ADMIN` or no `wireguard`
    /// kernel module, where trying the kernel backend first would just be
    /// a guaranteed, logged failure on every run.
    #[arg(long)]
    userspace: bool,

    /// The edge proxy's WireGuard pubkey — closes the gap flagged in this
    /// module's doc comment (previously: nothing told this agent's
    /// interface about the proxy at all, so no tunnel could form even with
    /// `CAP_NET_ADMIN`). Requires `--peer-endpoint`. A static pinned peer is
    /// the simplest thing that works for a single edge proxy; a fleet of
    /// proxies is future work (`docs/11` doesn't lock a discovery story for
    /// the origin's side of this yet).
    #[arg(long, requires = "peer_endpoint")]
    peer_pubkey: Option<String>,

    /// The edge proxy's public `ip:port` to dial. Required to actually
    /// initiate the handshake — an origin behind NAT can't rely on the
    /// proxy dialing in first, since the proxy only learns of an origin's
    /// endpoint from this agent's own (often NAT-invisible) `--endpoint`.
    #[arg(long, requires = "peer_pubkey")]
    peer_endpoint: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("GSP_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    for b in &args.backends {
        b.parse::<SocketAddr>()
            .with_context(|| format!("--backends entry {b:?} is not a valid ip:port"))?;
    }

    std::fs::create_dir_all(&args.data_dir)
        .with_context(|| format!("creating data dir {:?}", args.data_dir))?;
    let key_path = args.data_dir.join("private.key");
    let private_key = keypair::load_or_generate(&key_path)?;
    let pubkey = private_key.public_key().to_string();
    tracing::info!(pubkey = %pubkey, key_path = %key_path.display(), "wireguard identity ready");

    let address: IpAddrMask = args
        .address
        .parse()
        .map_err(|e| anyhow::anyhow!("--address {:?} is not a valid ip/cidr: {e}", args.address))?;

    let mut peers = Vec::new();
    if let (Some(peer_pubkey), Some(peer_endpoint)) = (&args.peer_pubkey, &args.peer_endpoint) {
        let public_key: Key = peer_pubkey
            .parse()
            .map_err(|e| anyhow::anyhow!("--peer-pubkey {peer_pubkey:?} is not valid: {e}"))?;
        let endpoint: SocketAddr = peer_endpoint
            .parse()
            .with_context(|| format!("--peer-endpoint {peer_endpoint:?} is not a valid ip:port"))?;
        let mut peer = Peer::new(public_key);
        peer.endpoint = Some(endpoint);
        // Single static peer (the edge proxy) — route everything through
        // it. A fleet of proxies per origin isn't a locked design yet
        // (`docs/11`), so this stays a single pinned peer for now.
        peer.allowed_ips = vec!["0.0.0.0/0".parse().unwrap()];
        // The proxy may sit behind NAT too (or this origin does) —
        // keepalive is what keeps the mapping alive between handshakes.
        peer.persistent_keepalive_interval = Some(25);
        tracing::info!(pubkey = %peer_pubkey, endpoint = %endpoint, "peering with the edge proxy");
        peers.push(peer);
    }

    let wg = interface::bring_up_with(
        &args.iface,
        &private_key,
        args.listen_port,
        address,
        peers,
        args.userspace,
    )
    .context("bringing up the local WireGuard interface")?;
    tracing::info!(iface = %args.iface, port = args.listen_port, "wireguard interface up");

    let client = reqwest::Client::new();
    tokio::spawn(register::run(
        client,
        args.controller_url.clone(),
        args.controller_token.clone(),
        register::Registration {
            name: args.name.clone(),
            pubkey,
            endpoint: args.endpoint.clone(),
            backends: args.backends.clone(),
        },
        Duration::from_secs(args.register_interval_sec),
    ));

    tokio::signal::ctrl_c()
        .await
        .context("waiting for a shutdown signal")?;
    tracing::info!("shutting down, removing the wireguard interface");
    if let Err(e) = wg.remove_interface() {
        tracing::warn!(error = %e, "failed to remove the wireguard interface cleanly");
    }
    Ok(())
}
