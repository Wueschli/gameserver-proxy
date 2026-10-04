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
mod live_interface;
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

    /// The pinned proxy's tunnel address (bare IPv4 or IPv6), routed as a
    /// `/32` or `/128`.
    #[arg(long, requires = "peer_pubkey")]
    peer_address: Option<String>,

    /// PEM file of extra CA certificates to trust for outbound HTTPS, in
    /// addition to the built-in Mozilla roots.
    #[arg(long)]
    ca_file: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("GSP_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    if let Some(path) = &args.ca_file {
        let certs = gsp_http::init_ca_file(path)?;
        tracing::info!(certs, path = %path.display(), "trusting extra CAs from --ca-file");
    }

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
        address_store::tunnel_ip(address_store::ip_of(c))
            .map_err(|e| anyhow::anyhow!("--address {c:?} must be an ip/prefix: {e}"))?;
    }
    let reg = register::Registration {
        name: args.name.clone(),
        pubkey: pubkey.clone(),
        endpoint: args.endpoint.clone(),
        backends: args.backends.clone(),
        address: pinned_cidr.map(|c| address_store::ip_of(c).to_string()),
    };
    let client = register::http_client();
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
        address_store::Source::Saved {
            ref cause,
            ref pin_ignored,
        } => {
            tracing::warn!(
                address = %start.cidr,
                error = %cause,
                "controller unreachable; starting with the last saved tunnel address"
            );
            if let Some(msg) = pin_ignored {
                tracing::warn!("{msg}");
            }
        }
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
        let peer_ip = address_store::tunnel_ip(peer_address)
            .map_err(|e| anyhow::anyhow!("--peer-address: {e}"))?;
        let mut peer = Peer::new(public_key);
        peer.endpoint = Some(endpoint);
        // A host route (`/32` or `/128`) to the pinned proxy's own tunnel
        // address — never 0.0.0.0/0, which would collide with every other
        // proxy's route.
        peer.allowed_ips = vec![IpAddrMask::host(peer_ip)];
        peer.persistent_keepalive_interval = Some(25);
        tracing::info!(pubkey = %peer_pubkey, endpoint = %endpoint, "peering with the edge proxy");
        peers.push(peer);
    }

    // A changed tunnel address rebuilds the interface (see `live_interface`),
    // so keep what that needs to bring it up again.
    let bring_up: live_interface::BringUp = {
        let (iface, key, port, userspace) = (
            args.iface.clone(),
            private_key.clone(),
            args.listen_port,
            args.userspace,
        );
        Box::new(move |address, peers| {
            interface::bring_up_with(&iface, &key, port, address, peers, userspace)
        })
    };
    let template = interface::config(
        &args.iface,
        &private_key,
        args.listen_port,
        address.clone(),
        Vec::new(),
    );
    let wg = Arc::new(live_interface::LiveInterface::new(
        bring_up(address.clone(), peers).context("bringing up the local WireGuard interface")?,
        address,
        bring_up,
        template,
    ));
    tracing::info!(iface = %args.iface, port = args.listen_port, "wireguard interface up");
    if let Some(ip) = args.peer_address.as_deref().and_then(|a| a.parse().ok()) {
        interface::kick_handshake(ip);
    }

    tokio::spawn(register::run(
        client,
        args.controller_url.clone(),
        args.controller_token.clone(),
        reg,
        Duration::from_secs(args.register_interval_sec),
        register::AddressSync {
            live: wg.clone(),
            pinned_cidr: pinned_cidr.map(str::to_string),
            path: addr_path,
        },
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
    if let Err(e) = wg.remove() {
        tracing::warn!(error = %e, "failed to remove the wireguard interface cleanly");
    }
    Ok(())
}
