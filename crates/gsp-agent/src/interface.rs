//! Brings up this origin's local WireGuard interface, per `docs/11`'s locked
//! decision: unmodified WireGuard via `defguard/wireguard-rs`, kernel module
//! primary with `boringtun` userspace as a portable fallback for
//! environments without `CAP_NET_ADMIN` (or no `wireguard` kernel module at
//! all). `defguard_wireguard_rs`'s `WGApi<Kernel>` / `WGApi<Userspace>` are
//! exactly this — the "unifies kernel-netlink and boringtun-userspace config
//! behind one API" ADR 25 called for, not two separate integrations.

use anyhow::Context;
use defguard_wireguard_rs::key::Key;
use defguard_wireguard_rs::net::IpAddrMask;
use defguard_wireguard_rs::peer::Peer;
use defguard_wireguard_rs::{
    InterfaceConfiguration, Kernel, Userspace, WGApi, WireguardInterfaceApi,
};

/// The tunnel interface MTU, set explicitly for both backends: the kernel
/// module defaults to 1420, but boringtun's TUN device comes up at 1500
/// (seen in the IPv6-underlay e2e), which leaves no room for WireGuard's
/// 80-byte overhead over an IPv6 underlay. 1420 also stays above IPv6's
/// 1280 minimum.
pub const TUNNEL_MTU: u32 = 1420;

/// Creates and configures the interface, trying the kernel backend first
/// unless `prefer_userspace` forces boringtun directly (for hosts known not
/// to have `CAP_NET_ADMIN` or the `wireguard` kernel module, where trying
/// the kernel backend first would just be a guaranteed failure logged on
/// every single run). Returns a boxed handle (`WGApi<Kernel>` and
/// `WGApi<Userspace>` are different concrete types — a trait object is the
/// only way the caller can hold "whichever one actually came up" without
/// knowing which at compile time) so the caller can later remove the
/// interface on shutdown regardless of which backend won. `+ Send + Sync`
/// (matching `gsp::tunnel_client::bring_up`'s identical bound) so the same
/// handle can be shared with `proxy_subscribe`'s background reconcile task
/// (phase 14 slice 7) via an `Arc`.
pub fn bring_up_with(
    ifname: &str,
    private_key: &Key,
    listen_port: u16,
    address: IpAddrMask,
    peers: Vec<Peer>,
    prefer_userspace: bool,
) -> anyhow::Result<Box<dyn WireguardInterfaceApi + Send + Sync>> {
    let config = InterfaceConfiguration {
        name: ifname.to_string(),
        prvkey: private_key.to_string(),
        addresses: vec![address],
        port: listen_port,
        peers,
        mtu: Some(TUNNEL_MTU),
        fwmark: None,
    };

    if !prefer_userspace {
        match configure::<Kernel>(ifname, &config) {
            Ok(api) => return Ok(Box::new(api)),
            Err(e) => tracing::warn!(
                error = %e,
                "kernel WireGuard interface unavailable, falling back to boringtun userspace"
            ),
        }
    }

    let api = configure::<Userspace>(ifname, &config)
        .context("bringing up the boringtun userspace WireGuard interface")?;
    Ok(Box::new(api))
}

fn configure<API>(ifname: &str, config: &InterfaceConfiguration) -> anyhow::Result<WGApi<API>>
where
    WGApi<API>: WireguardInterfaceApi + Send + Sync,
{
    let mut api = WGApi::<API>::new(ifname.to_string())
        .with_context(|| format!("creating a WGApi handle for interface {ifname:?}"))?;
    api.create_interface()
        .with_context(|| format!("creating interface {ifname:?}"))?;
    api.configure_interface(config)
        .with_context(|| format!("configuring interface {ifname:?}"))?;
    Ok(api)
}
