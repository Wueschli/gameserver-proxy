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

/// UDP port the handshake trigger is aimed at (the discard service; nothing
/// listens, and nothing needs to).
const KICK_PORT: u16 = 9;

/// Sends one empty datagram to `peer` through the tunnel so the backend starts
/// a handshake **now**. `boringtun` arms a peer's persistent keepalive only
/// 25 s after the peer is created, and the edge proxy has no endpoint for this
/// origin, so without a trigger the first handshake waits that whole interval
/// (the kernel backend sends a keepalive the moment it is configured, ~2 s).
/// Best-effort: the keepalive still covers a failure here.
pub fn kick_handshake(peer: std::net::IpAddr) {
    kick_handshake_at(std::net::SocketAddr::new(peer, KICK_PORT));
}

fn kick_handshake_at(target: std::net::SocketAddr) {
    let bind: std::net::SocketAddr = match target {
        std::net::SocketAddr::V4(_) => ([0, 0, 0, 0], 0).into(),
        std::net::SocketAddr::V6(_) => (std::net::Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    let sent = std::net::UdpSocket::bind(bind).and_then(|s| s.send_to(&[], target));
    if let Err(e) = sent {
        tracing::debug!(%target, error = %e, "could not trigger an immediate WireGuard handshake");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kick_handshake_sends_one_empty_datagram_to_the_target() {
        let rx = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        kick_handshake_at(rx.local_addr().unwrap());
        let mut buf = [0u8; 8];
        let (n, _) = rx.recv_from(&mut buf).expect("a datagram should arrive");
        assert_eq!(n, 0);
    }

    #[test]
    fn kick_handshake_swallows_an_unreachable_target() {
        // No route / refused must never panic or propagate.
        kick_handshake("192.0.2.1".parse().unwrap());
    }
}
