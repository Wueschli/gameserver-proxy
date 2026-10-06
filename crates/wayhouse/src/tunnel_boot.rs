//! `--tunnel-*` bring-up (phase 14 slice 4, `docs/11`), behind the `tunnel` cargo
//! feature; `tunnel_boot_disabled.rs` stands in without it.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;

use crate::tunnel_source::TunnelRegistry;
use crate::Args;

/// Resolved `--tunnel-*` settings, built once in `async_main` after
/// validating the flag combination — `start` doesn't need to re-check
/// `tunnel_address`/`tunnel_controller_url` are `Some` a second time.
pub struct TunnelConfig {
    iface: String,
    listen_port: u16,
    address: Option<String>,
    key_file: PathBuf,
    controller_url: String,
    controller_token: Option<String>,
    userspace: bool,
    name: String,
    endpoint: String,
    register_interval: Duration,
}

/// The `--tunnel-*` settings, or `None` when `--tunnel-iface` is not given.
pub fn config(args: &Args) -> anyhow::Result<Option<TunnelConfig>> {
    let Some(iface) = &args.tunnel_iface else {
        return Ok(None);
    };
    let controller_url = args
        .tunnel_controller_url
        .clone()
        .ok_or_else(|| anyhow::anyhow!("--tunnel-iface requires --tunnel-controller-url"))?;
    let name = args
        .tunnel_name
        .clone()
        .ok_or_else(|| anyhow::anyhow!("--tunnel-iface requires --tunnel-name"))?;
    let endpoint = args
        .tunnel_endpoint
        .clone()
        .ok_or_else(|| anyhow::anyhow!("--tunnel-iface requires --tunnel-endpoint"))?;
    Ok(Some(TunnelConfig {
        iface: iface.clone(),
        listen_port: args.tunnel_listen_port,
        address: args.tunnel_address.clone(),
        key_file: args.tunnel_key_file.clone(),
        controller_url,
        controller_token: args.tunnel_controller_token.clone(),
        userspace: args.tunnel_userspace,
        name,
        endpoint,
        register_interval: Duration::from_secs(args.tunnel_register_interval_sec),
    }))
}

/// A `tunnel` backend source resolves an origin's currently-registered backends
/// from the same backend-peers registry `start`'s reconcile task subscribes to,
/// so it reuses `--tunnel-controller-url`/`--tunnel-controller-token`.
pub fn registry(tc: &TunnelConfig) -> TunnelRegistry {
    TunnelRegistry {
        controller_url: tc.controller_url.clone(),
        token: tc.controller_token.clone(),
    }
}

/// The running tunnel: its two background tasks and the live interface.
pub struct Running {
    reconcile: tokio::task::JoinHandle<()>,
    register: tokio::task::JoinHandle<()>,
    live: Arc<crate::live_interface::LiveInterface>,
}

impl Running {
    pub fn stop(self) {
        self.reconcile.abort();
        self.register.abort();
        if let Err(e) = self.live.remove() {
            tracing::warn!(error = %e, "failed to remove the wireguard tunnel interface cleanly");
        }
    }
}

pub async fn start(tc: TunnelConfig) -> anyhow::Result<Running> {
    let private_key = crate::tunnel_client::load_or_generate_key(&tc.key_file)
        .with_context(|| format!("loading tunnel key from {:?}", tc.key_file))?;
    let pubkey = private_key.public_key().to_string();

    // The controller is the address authority: register BEFORE the
    // interface exists (the answer is its address) and before any
    // listener binds — a failure here is a failing `--tunnel-*`.
    let pinned_cidr = tc.address.clone();
    if let Some(c) = pinned_cidr.as_deref() {
        crate::tunnel_address::tunnel_ip(crate::tunnel_address::ip_of(c))
            .map_err(|e| anyhow::anyhow!("--tunnel-address {c:?} must be an ip/prefix: {e}"))?;
    }
    let reg = crate::proxy_register::Registration {
        name: tc.name.clone(),
        pubkey,
        endpoint: tc.endpoint.clone(),
        address: pinned_cidr
            .as_deref()
            .map(|c| crate::tunnel_address::ip_of(c).to_string()),
        boot_id: crate::proxy_register::new_boot_id(),
        refresh_sec: tc.register_interval.as_secs(),
    };
    let client = crate::proxy_register::http_client();
    let addr_path = {
        let mut p = tc.key_file.clone().into_os_string();
        p.push(".address");
        PathBuf::from(p)
    };
    let outcome = crate::proxy_register::register_with_retry(
        &client,
        &tc.controller_url,
        tc.controller_token.as_deref(),
        &reg,
        Duration::from_secs(30),
    )
    .await;
    let start = crate::tunnel_address::resolve_startup(
        outcome,
        pinned_cidr.as_deref(),
        crate::tunnel_address::load(&addr_path),
    )?;
    match start.source {
        crate::tunnel_address::Source::Controller => {
            crate::tunnel_address::save(&addr_path, &start.cidr)?
        }
        crate::tunnel_address::Source::Saved {
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
    let address: defguard_wireguard_rs::net::IpAddrMask = start
        .cidr
        .parse()
        .map_err(|e| anyhow::anyhow!("tunnel address {:?} is invalid: {e}", start.cidr))?;
    let wg: Arc<dyn defguard_wireguard_rs::WireguardInterfaceApi + Send + Sync> =
        Arc::from(crate::tunnel_client::bring_up(
            &tc.iface,
            &private_key,
            tc.listen_port,
            address.clone(),
            tc.userspace,
        )?);
    let live = Arc::new(crate::live_interface::LiveInterface::new(
        wg,
        address,
        crate::netlink_addr::deleter(tc.iface.clone()),
        crate::netlink_addr::link_deleter(tc.iface.clone()),
    ));
    tracing::info!(
        iface = %tc.iface,
        port = tc.listen_port,
        address = %start.cidr,
        pubkey = %private_key.public_key(),
        controller = %tc.controller_url,
        "wireguard tunnel interface up; subscribing to backend-peers updates"
    );
    let task = tokio::spawn(crate::tunnel_client::run(
        tc.controller_url.clone(),
        tc.controller_token.clone(),
        live.api(),
    ));
    // Register ourselves (periodically) so every origin's `wayhouse-agent`
    // can peer with us — the mirror image of `task` above.
    let register_task = tokio::spawn(crate::proxy_register::run(
        client,
        tc.controller_url,
        tc.controller_token,
        reg,
        tc.register_interval,
        crate::proxy_register::AddressSync {
            live: live.clone(),
            pinned_cidr,
            path: addr_path,
        },
    ));
    Ok(Running {
        reconcile: task,
        register: register_task,
        live,
    })
}
