//! `--tunnel-*` — phase 14 slice 4 (`docs/08` "Phase 14", `docs/11-backend-
//! transport.md`): the proxy side of the WireGuard backend transport.
//! Brings up this proxy's own shared WireGuard interface ([`bring_up`] —
//! one shared interface with many peers, `docs/11` "Locked decisions" #2,
//! not one interface per origin) and holds a long-lived `GET
//! /peers/subscribe` connection against `gsp-controller`'s backend-peers
//! registry (phase 14 slice 2), reconciling every registered origin onto
//! this interface's peer list (skipping a registration that hasn't changed
//! since the last one applied — see [`run`]'s doc for why that isn't just
//! an optimization).
//!
//! Mirrors `controller_client.rs`'s subscribe/reconnect-with-backoff shape
//! almost exactly — a dropped connection just retries with capped
//! exponential backoff, and the interface itself (like the last-applied
//! `Snapshot`) is left exactly as it was, never torn down, while
//! disconnected (`docs/10` principle 4, "freeze on last-known-good,
//! applied here to WireGuard peers instead of routing config").
//!
//! **Scope of this slice**: this task only ever adds/updates a peer — it
//! never removes one, even if an origin stops registering. A registration
//! is asserted (`docs/10`'s framing: "last known good"), and there's no
//! signal here yet for "this origin is gone for good" vs. "temporarily
//! unreachable"; removal is left for a later pass once that distinction is
//! designed. `endpoint` is only ever set when the origin's registration
//! carries one (i.e. this proxy could dial out); the common case — an
//! origin behind a home NAT — leaves the peer endpoint-less, and this
//! proxy passively waits for the origin's own `gsp-agent` to initiate the
//! WireGuard handshake (`gsp-agent --peer-endpoint/--peer-pubkey`, verified
//! end to end in slice 6 — a static pin, since the proxy already always has
//! a known public address in this design, so there's no discovery problem
//! on that side).

use std::path::Path;
use std::time::Duration;

use anyhow::Context;
use defguard_wireguard_rs::key::Key;
use defguard_wireguard_rs::net::IpAddrMask;
use defguard_wireguard_rs::peer::Peer;
use defguard_wireguard_rs::{
    InterfaceConfiguration, Kernel, Userspace, WGApi, WireguardInterfaceApi,
};
use serde::Deserialize;

const RECONNECT_MIN: Duration = Duration::from_millis(500);
const RECONNECT_MAX: Duration = Duration::from_secs(30);

#[cfg(unix)]
fn restrict_permissions(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restricting permissions on {}", path.display()))
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

/// Loads this proxy's own WireGuard private key, or generates and persists
/// one on first run — the identical shape `gsp-agent::keypair` uses,
/// duplicated rather than shared (there is no `gsp-agent` library to depend
/// on; it's a binary crate, same as this one). A stable identity matters
/// less on this side (an origin's peer entry for the proxy is configured
/// statically by whoever runs `gsp-agent`, not learned from a registry —
/// see the module doc), but persisting still avoids a pointless key churn
/// on every restart.
pub fn load_or_generate_key(path: &Path) -> anyhow::Result<Key> {
    if path.exists() {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading private key from {}", path.display()))?;
        return Key::try_from(text.trim()).map_err(|e| {
            anyhow::anyhow!(
                "{} does not hold a valid WireGuard key: {e}",
                path.display()
            )
        });
    }

    let key = Key::generate();
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("creating private key file {}", path.display()))?;
    use std::io::Write;
    file.write_all(key.to_string().as_bytes())
        .with_context(|| format!("writing private key to {}", path.display()))?;
    drop(file);
    restrict_permissions(path)?;
    Ok(key)
}

/// Brings up this proxy's shared WireGuard interface — the same
/// kernel-primary, boringtun-fallback shape `gsp-agent::interface` uses.
pub fn bring_up(
    ifname: &str,
    private_key: &Key,
    listen_port: u16,
    address: IpAddrMask,
    prefer_userspace: bool,
) -> anyhow::Result<Box<dyn WireguardInterfaceApi + Send + Sync>> {
    let config = InterfaceConfiguration {
        name: ifname.to_string(),
        prvkey: private_key.to_string(),
        addresses: vec![address],
        port: listen_port,
        peers: Vec::new(),
        mtu: None,
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

/// One origin's registration, exactly as `gsp-controller::peers::
/// PeerRegistration` serializes it (duplicated wire shape — same precedent
/// `controller_client`'s hand-parsed SSE and `aggregator_client`'s
/// duplicated `IngestPayload` already established).
#[derive(Debug, Deserialize, PartialEq)]
struct PeerRegistration {
    name: String,
    pubkey: String,
    #[serde(default)]
    endpoint: Option<String>,
    #[serde(default)]
    backends: Vec<String>,
}

/// Builds the WireGuard peer this registration implies: `allowed_ips` is
/// every backend address as a `/32` (or `/128`) host route — exactly the
/// tunnel-internal addresses this origin fronts, nothing wider — and
/// `endpoint` is only set when the registration carries one (see the
/// module doc on why that's the common-case-absent field, not a bug).
fn to_wg_peer(reg: &PeerRegistration) -> anyhow::Result<Peer> {
    let key = Key::try_from(reg.pubkey.as_str())
        .map_err(|e| anyhow::anyhow!("origin {:?} has an invalid pubkey: {e}", reg.name))?;
    let mut peer = Peer::new(key);

    let mut allowed_ips = Vec::with_capacity(reg.backends.len());
    for b in &reg.backends {
        let addr: std::net::SocketAddr = b.parse().map_err(|e| {
            anyhow::anyhow!(
                "origin {:?} backend {b:?} is not a valid ip:port: {e}",
                reg.name
            )
        })?;
        allowed_ips.push(IpAddrMask::host(addr.ip()));
    }
    peer.set_allowed_ips(allowed_ips);

    if let Some(endpoint) = &reg.endpoint {
        peer.set_endpoint(endpoint).map_err(|e| {
            anyhow::anyhow!(
                "origin {:?} endpoint {endpoint:?} is invalid: {e}",
                reg.name
            )
        })?;
    }
    Ok(peer)
}

/// Runs forever, reconnecting with backoff on disconnect — mirrors
/// `controller_client::run`'s shape. Always subscribes from `since=0`: this
/// task holds no persisted local revision cursor across a reconnect, but it
/// does hold `last_applied` (this origin's last-applied registration, by
/// name) across reconnects and across every event on a live connection —
/// found necessary by live end-to-end testing (`docs/08` phase 14 slice 6):
/// [`reconcile_peer`] removes-then-adds the peer (see its doc for why), and
/// `gsp-agent` re-registers on a fixed interval regardless of whether
/// anything changed, so without this skip a stable tunnel would never stay
/// up — every re-registration would tear down the handshake the previous
/// one just completed.
pub async fn run(
    controller_url: String,
    token: Option<String>,
    wg: std::sync::Arc<dyn WireguardInterfaceApi + Send + Sync>,
) {
    let mut backoff = RECONNECT_MIN;
    let mut last_applied: std::collections::HashMap<String, PeerRegistration> =
        std::collections::HashMap::new();
    loop {
        match subscribe_once(
            &controller_url,
            token.as_deref(),
            wg.as_ref(),
            &mut last_applied,
        )
        .await
        {
            Ok(()) => {
                backoff = RECONNECT_MIN;
                tracing::warn!(
                    controller = %controller_url,
                    "backend-peers subscribe stream ended; reconnecting"
                );
            }
            Err(e) => {
                tracing::warn!(
                    error = %e, controller = %controller_url,
                    "backend-peers subscribe connection failed; keeping the current \
                     WireGuard peer set and retrying"
                );
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(RECONNECT_MAX);
    }
}

async fn subscribe_once(
    base_url: &str,
    token: Option<&str>,
    wg: &(dyn WireguardInterfaceApi + Send + Sync),
    last_applied: &mut std::collections::HashMap<String, PeerRegistration>,
) -> anyhow::Result<()> {
    let url = format!("{base_url}/peers/subscribe");
    let mut req = reqwest::Client::new().get(&url);
    if let Some(token) = token {
        req = req.bearer_auth(token);
    }
    let mut resp = req
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("connecting to {url}: {e}"))?;
    if !resp.status().is_success() {
        anyhow::bail!("controller {url} returned {}", resp.status());
    }
    tracing::info!(controller = %base_url, "subscribed to backend-peers updates");

    let mut buf = String::new();
    loop {
        let chunk = resp
            .chunk()
            .await
            .map_err(|e| anyhow::anyhow!("reading subscribe stream from {base_url}: {e}"))?;
        let Some(bytes) = chunk else {
            return Ok(()); // server closed the stream
        };
        buf.push_str(&String::from_utf8_lossy(&bytes));

        while let Some(end) = buf.find("\n\n") {
            let event = buf[..end].to_string();
            buf.drain(..end + 2);
            if let Some(reg) = parse_sse_event(&event) {
                if last_applied.get(&reg.name) == Some(&reg) {
                    continue; // unchanged since last apply — don't churn the session
                }
                let name = reg.name.clone();
                reconcile_peer(wg, &reg);
                last_applied.insert(name, reg);
            }
        }
    }
}

/// Parses one SSE event block (`gsp-controller`'s `data:
/// {"revision":N,"registration":{...}}` shape). `None` for anything that
/// isn't a data event — a keep-alive comment block, or a malformed one —
/// exactly like `controller_client::parse_sse_event`'s posture: the
/// connection is still healthy, there's simply nothing to reconcile.
fn parse_sse_event(event: &str) -> Option<PeerRegistration> {
    let data_line = event
        .split('\n')
        .find_map(|line| line.strip_prefix("data:"))?
        .trim_start();
    let payload: serde_json::Value = serde_json::from_str(data_line).ok()?;
    serde_json::from_value(payload.get("registration")?.clone()).ok()
}

/// Removes any existing peer under this pubkey before adding it back with
/// the latest fields — found by live end-to-end testing (`docs/08` phase 14
/// slice 6) to be required, not just defensive: `defguard_boringtun`'s
/// userspace backend panics ("Modifying existing peers is not yet
/// supported") if `configure_peer` is called for a pubkey it already has,
/// unlike the kernel backend's netlink upsert. `remove_peer` on a pubkey
/// that isn't configured yet is a harmless no-op on both backends, so this
/// is safe to do unconditionally on every registration, including the
/// first one for a given origin.
fn reconcile_peer(wg: &(dyn WireguardInterfaceApi + Send + Sync), reg: &PeerRegistration) {
    match to_wg_peer(reg) {
        Ok(peer) => {
            if let Err(e) = wg.remove_peer(&peer.public_key) {
                tracing::debug!(
                    origin = %reg.name, error = %e,
                    "removing any existing wireguard peer before reconfiguring it \
                     (harmless if it wasn't configured yet)"
                );
            }
            match wg.configure_peer(&peer) {
                Ok(()) => tracing::info!(
                    origin = %reg.name,
                    backends = ?reg.backends,
                    has_endpoint = reg.endpoint.is_some(),
                    "reconciled wireguard peer for origin"
                ),
                Err(e) => tracing::error!(
                    origin = %reg.name, error = %e,
                    "failed to configure wireguard peer for origin"
                ),
            }
        }
        Err(e) => tracing::error!(
            origin = %reg.name, error = %e,
            "skipping a malformed peer registration"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_well_formed_data_event() {
        let event = r#"data: {"revision":1,"registration":{"name":"home","pubkey":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=","endpoint":"203.0.113.7:51820","backends":["10.60.0.2:1"]}}"#;
        let reg = parse_sse_event(event).unwrap();
        assert_eq!(reg.name, "home");
        assert_eq!(reg.endpoint.as_deref(), Some("203.0.113.7:51820"));
        assert_eq!(reg.backends, vec!["10.60.0.2:1"]);
    }

    #[test]
    fn a_keep_alive_comment_block_is_not_a_data_event() {
        assert!(parse_sse_event(": keep-alive").is_none());
    }

    #[test]
    fn malformed_json_is_ignored_not_a_panic() {
        assert!(parse_sse_event("data: not json").is_none());
    }

    #[test]
    fn a_registration_missing_a_required_field_is_ignored() {
        assert!(parse_sse_event(r#"data: {"revision":1,"registration":{"pubkey":"x"}}"#).is_none());
    }

    #[test]
    fn to_wg_peer_rejects_a_malformed_pubkey() {
        let reg = PeerRegistration {
            name: "home".into(),
            pubkey: "not-a-key".into(),
            endpoint: None,
            backends: vec![],
        };
        assert!(to_wg_peer(&reg).is_err());
    }

    #[test]
    fn to_wg_peer_rejects_a_malformed_backend_address() {
        let reg = PeerRegistration {
            name: "home".into(),
            pubkey: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
            endpoint: None,
            backends: vec!["not-an-addr".into()],
        };
        assert!(to_wg_peer(&reg).is_err());
    }

    #[test]
    fn to_wg_peer_builds_host_allowed_ips_from_backends() {
        let reg = PeerRegistration {
            name: "home".into(),
            pubkey: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
            endpoint: Some("203.0.113.7:51820".into()),
            backends: vec!["10.60.0.2:25565".into(), "10.60.0.3:25566".into()],
        };
        let peer = to_wg_peer(&reg).unwrap();
        assert_eq!(peer.allowed_ips.len(), 2);
        assert!(peer.allowed_ips.iter().all(|ip| ip.cidr == 32));
        assert!(peer.endpoint.is_some());
    }

    #[test]
    fn to_wg_peer_leaves_endpoint_unset_when_the_registration_has_none() {
        let reg = PeerRegistration {
            name: "home".into(),
            pubkey: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
            endpoint: None,
            backends: vec![],
        };
        let peer = to_wg_peer(&reg).unwrap();
        assert!(peer.endpoint.is_none());
    }
}
