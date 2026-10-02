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
//! tells this agent about itself. So this agent also subscribes to
//! `gsp-controller`'s proxy-peers registry ([`proxy_subscribe`], phase 14
//! slice 7) and reconciles every registered proxy onto this interface, the
//! mirror image of `gsp`'s own subscribe-and-reconcile task — a growing
//! proxy fleet, or one added after this origin was deployed, needs no
//! restart here. `--peer-pubkey`/`--peer-endpoint` still exist alongside
//! it as a manual pin (handy for a bootstrap proxy or a deployment too
//! small to bother with the registry) — see [`proxy_subscribe`]'s module
//! doc for why the two don't conflict.

mod address_store;
mod interface;
mod keypair;
mod proxy_subscribe;
mod register;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
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

    /// This interface's own tunnel-internal address as `ip/prefix`
    /// (e.g. `10.60.0.2/24`) to **pin** it. Omit to have the controller allocate
    /// one from its `--tunnel-network` (it is returned on registration and
    /// saved to `<data-dir>/tunnel-address`).
    #[arg(long)]
    address: Option<String>,

    /// Backend addresses this origin fronts: `host:port`, or the shorthand
    /// `:port` ("my tunnel address plus this port"). Every host must be this
    /// origin's own tunnel address.
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

    /// A manually-pinned edge proxy's WireGuard pubkey, in addition to whatever
    /// the proxy-peers registry supplies. Requires `--peer-endpoint` and
    /// `--peer-address`.
    #[arg(long, requires_all = ["peer_endpoint", "peer_address"])]
    peer_pubkey: Option<String>,

    /// The edge proxy's public `ip:port` to dial.
    #[arg(long, requires = "peer_pubkey")]
    peer_endpoint: Option<String>,

    /// The pinned proxy's tunnel address (bare IPv4), routed as a `/32`.
    #[arg(long, requires = "peer_pubkey")]
    peer_address: Option<String>,
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
        let ok = match b.strip_prefix(':') {
            Some(port) => port.parse::<u16>().is_ok(),
            None => b.parse::<SocketAddr>().is_ok(),
        };
        anyhow::ensure!(ok, "--backends entry {b:?} is not host:port or :port");
    }

    std::fs::create_dir_all(&args.data_dir)
        .with_context(|| format!("creating data dir {:?}", args.data_dir))?;
    let key_path = args.data_dir.join("private.key");
    let private_key = keypair::load_or_generate(&key_path)?;
    let pubkey = private_key.public_key().to_string();
    tracing::info!(pubkey = %pubkey, key_path = %key_path.display(), "wireguard identity ready");

    // The controller is the address authority: register BEFORE the interface
    // exists, because the answer is the interface's address.
    let pinned_cidr = args.address.as_deref();
    if let Some(c) = pinned_cidr {
        address_store::ip_of(c)
            .parse::<std::net::Ipv4Addr>()
            .with_context(|| format!("--address {c:?} must be an IPv4 ip/prefix"))?;
    }
    let reg = register::Registration {
        name: args.name.clone(),
        pubkey: pubkey.clone(),
        endpoint: args.endpoint.clone(),
        backends: args.backends.clone(),
        address: pinned_cidr.map(|c| address_store::ip_of(c).to_string()),
    };
    let client = reqwest::Client::new();
    let addr_path = args.data_dir.join("tunnel-address");
    let outcome = register::register_with_retry(
        &client,
        &args.controller_url,
        args.controller_token.as_deref(),
        &reg,
        Duration::from_secs(30),
    )
    .await;
    let start =
        address_store::resolve_startup(outcome, pinned_cidr, address_store::load(&addr_path))?;
    match start.source {
        address_store::Source::Controller => address_store::save(&addr_path, &start.cidr)?,
        address_store::Source::Saved => tracing::warn!(
            address = %start.cidr,
            "controller unreachable; starting with the last saved tunnel address"
        ),
    }
    tracing::info!(address = %start.cidr, "tunnel address ready");
    let address: IpAddrMask = start.cidr.parse().map_err(|e| {
        anyhow::anyhow!(
            "tunnel address {:?} is not a valid ip/cidr: {e}",
            start.cidr
        )
    })?;

    let mut peers = Vec::new();
    if let (Some(peer_pubkey), Some(peer_endpoint), Some(peer_address)) =
        (&args.peer_pubkey, &args.peer_endpoint, &args.peer_address)
    {
        let public_key: Key = peer_pubkey
            .parse()
            .map_err(|e| anyhow::anyhow!("--peer-pubkey {peer_pubkey:?} is not valid: {e}"))?;
        let endpoint: SocketAddr = peer_endpoint
            .parse()
            .with_context(|| format!("--peer-endpoint {peer_endpoint:?} is not a valid ip:port"))?;
        let peer_ip: std::net::Ipv4Addr = peer_address
            .parse()
            .with_context(|| format!("--peer-address {peer_address:?} is not an IPv4 address"))?;
        let mut peer = Peer::new(public_key);
        peer.endpoint = Some(endpoint);
        // A host route to the pinned proxy's own tunnel address — never
        // 0.0.0.0/0, which would collide with every other proxy's route.
        peer.allowed_ips = vec![format!("{peer_ip}/32").parse().unwrap()];
        peer.persistent_keepalive_interval = Some(25);
        tracing::info!(pubkey = %peer_pubkey, endpoint = %endpoint, "peering with the edge proxy");
        peers.push(peer);
    }

    let wg: Arc<dyn defguard_wireguard_rs::WireguardInterfaceApi + Send + Sync> = Arc::from(
        interface::bring_up_with(
            &args.iface,
            &private_key,
            args.listen_port,
            address,
            peers,
            args.userspace,
        )
        .context("bringing up the local WireGuard interface")?,
    );
    tracing::info!(iface = %args.iface, port = args.listen_port, "wireguard interface up");

    tokio::spawn(register::run(
        client,
        args.controller_url.clone(),
        args.controller_token.clone(),
        reg,
        Duration::from_secs(args.register_interval_sec),
        address_store::ip_of(&start.cidr).to_string(),
    ));
    // Phase 14 slice 7: learn about every edge proxy, not just a manually
    // pinned one — see `proxy_subscribe`'s module doc.
    let subscribe_task = tokio::spawn(proxy_subscribe::run(
        args.controller_url.clone(),
        args.controller_token.clone(),
        wg.clone(),
    ));

    tokio::signal::ctrl_c()
        .await
        .context("waiting for a shutdown signal")?;
    tracing::info!("shutting down, removing the wireguard interface");
    subscribe_task.abort();
    if let Err(e) = wg.remove_interface() {
        tracing::warn!(error = %e, "failed to remove the wireguard interface cleanly");
    }
    Ok(())
}
