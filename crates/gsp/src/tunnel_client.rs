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
//! **Removal**: an origin's deletion arrives as a controller tombstone
//! (`removed`) and removes the matching WireGuard peer; silence alone never
//! does ("last known good"). Each origin is routed as one `/32` (or IPv6 `/128`) host route to
//! its controller-assigned tunnel address. `endpoint` is only ever set when the origin's registration
//! carries one (i.e. this proxy could dial out); the common case — an
//! origin behind a home NAT — leaves the peer endpoint-less, and this
//! proxy passively waits for the origin's own `gsp-agent` to initiate the
//! WireGuard handshake (`gsp-agent --peer-endpoint/--peer-pubkey`, verified
//! end to end in slice 6 — a static pin, since the proxy already always has
//! a known public address in this design, so there's no discovery problem
//! on that side).

use std::collections::HashMap;
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
    use std::io::Write;
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
    file.write_all(key.to_string().as_bytes())
        .with_context(|| format!("writing private key to {}", path.display()))?;
    drop(file);
    restrict_permissions(path)?;
    Ok(key)
}

/// The tunnel interface MTU, set explicitly for both backends: the kernel
/// module defaults to 1420, but boringtun's TUN device comes up at 1500
/// (seen in the IPv6-underlay e2e), which leaves no room for WireGuard's
/// 80-byte overhead over an IPv6 underlay. 1420 also stays above IPv6's
/// 1280 minimum.
pub const TUNNEL_MTU: u32 = 1420;

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

/// One origin's registration, exactly as `gsp-controller::peers::
/// PeerRegistration` serializes it (duplicated wire shape — same precedent
/// `controller_client`'s hand-parsed SSE and `aggregator_client`'s duplicated
/// `IngestPayload` already established).
#[derive(Debug, Clone, Deserialize, PartialEq)]
struct PeerRegistration {
    name: String,
    pubkey: String,
    #[serde(default)]
    endpoint: Option<String>,
    #[serde(default)]
    backends: Vec<String>,
    #[serde(default)]
    tunnel_address: Option<String>,
}

/// Builds the WireGuard peer this registration implies: a **host route to the
/// origin's own tunnel address** (`/32` or `/128`) — the controller guarantees it is
/// unique and that every backend lives on it — and `endpoint` only when the
/// registration carries one (the common-case-absent field, not a bug).
fn to_wg_peer(reg: &PeerRegistration) -> anyhow::Result<Peer> {
    let key = Key::try_from(reg.pubkey.as_str())
        .map_err(|e| anyhow::anyhow!("origin {:?} has an invalid pubkey: {e}", reg.name))?;
    let addr = reg.tunnel_address.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "origin {:?} has no tunnel_address (is the controller up to date?)",
            reg.name
        )
    })?;
    let ip: std::net::IpAddr = addr.parse().map_err(|e| {
        anyhow::anyhow!(
            "origin {:?} tunnel_address {addr:?} is invalid: {e}",
            reg.name
        )
    })?;
    let mut peer = Peer::new(key);
    // `/32` for IPv4, `/128` for IPv6.
    peer.set_allowed_ips(vec![IpAddrMask::host(ip)]);
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
    let mut last_applied: HashMap<String, PeerRegistration> = HashMap::new();
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
    last_applied: &mut HashMap<String, PeerRegistration>,
) -> anyhow::Result<()> {
    let url = format!("{base_url}/peers/subscribe");
    let mut req = gsp_http::client().get(&url);
    if let Some(token) = token {
        req = req.bearer_auth(token);
    }
    let mut resp = req
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("connecting to {url}: {}", gsp_http::error_chain(&e)))?;
    if !resp.status().is_success() {
        anyhow::bail!("controller {url} returned {}", resp.status());
    }
    tracing::info!(controller = %base_url, "subscribed to backend-peers updates");

    let mut buf = gsp_http::sse::EventBuffer::new();
    loop {
        let chunk = resp.chunk().await.map_err(|e| {
            anyhow::anyhow!(
                "reading subscribe stream from {base_url}: {}",
                gsp_http::error_chain(&e)
            )
        })?;
        let Some(bytes) = chunk else {
            return Ok(()); // server closed the stream
        };
        buf.push(&bytes)
            .map_err(|e| anyhow::anyhow!("subscribe stream from {base_url}: {e}"))?;

        while let Some(event) = buf.next_event() {
            if let Some(ev) = parse_sse_event(&event) {
                match plan(last_applied, &ev) {
                    Action::Skip => {}
                    Action::Reconcile(reg) => {
                        if let Some(old) = replaced_pubkey(last_applied, reg) {
                            remove_peer(wg, &reg.name, old);
                        }
                        reconcile_peer(wg, reg);
                        last_applied.insert(reg.name.clone(), reg.clone());
                    }
                    Action::Remove(pubkey) => {
                        if let Event::Removed(name) = &ev {
                            remove_peer(wg, name, &pubkey);
                            last_applied.remove(name);
                        }
                    }
                }
            }
        }
    }
}

#[derive(Debug, PartialEq)]
enum Event {
    Registered(PeerRegistration),
    Removed(String),
}

/// Parses one SSE event block — identical shape to
/// `gsp-agent::proxy_subscribe::parse_sse_event`.
fn parse_sse_event(event: &str) -> Option<Event> {
    let data_line = event
        .split('\n')
        .find_map(|line| line.strip_prefix("data:"))?
        .trim_start();
    let payload: serde_json::Value = serde_json::from_str(data_line).ok()?;
    if let Some(name) = payload
        .get("removed")
        .and_then(|r| r.get("name"))
        .and_then(|n| n.as_str())
    {
        return Some(Event::Removed(name.to_string()));
    }
    serde_json::from_value(payload.get("registration")?.clone())
        .ok()
        .map(Event::Registered)
}

#[derive(Debug, PartialEq)]
enum Action<'a> {
    Skip,
    Reconcile(&'a PeerRegistration),
    /// Remove the WireGuard peer with this pubkey.
    Remove(String),
}

/// What to do with `event` given what is already applied — pure, so the
/// catch-up replay (add, then removal) is testable without a WireGuard device.
fn plan<'a>(applied: &HashMap<String, PeerRegistration>, event: &'a Event) -> Action<'a> {
    match event {
        Event::Registered(reg) if applied.get(&reg.name) == Some(reg) => Action::Skip,
        Event::Registered(reg) => Action::Reconcile(reg),
        Event::Removed(name) => match applied.get(name) {
            Some(old) => Action::Remove(old.pubkey.clone()),
            None => Action::Skip,
        },
    }
}

/// The pubkey of a peer that `reg` supersedes: the same name now registered
/// under a different key. `reconcile_peer` only touches the new key's peer, so
/// without removing this one the old key would stay a live peer (with its
/// routes) until the interface is torn down.
fn replaced_pubkey<'a>(
    applied: &'a HashMap<String, PeerRegistration>,
    reg: &PeerRegistration,
) -> Option<&'a str> {
    applied
        .get(&reg.name)
        .map(|old| old.pubkey.as_str())
        .filter(|old| *old != reg.pubkey)
}

fn remove_peer(wg: &(dyn WireguardInterfaceApi + Send + Sync), name: &str, pubkey: &str) {
    match Key::try_from(pubkey) {
        Ok(key) => match wg.remove_peer(&key) {
            Ok(()) => tracing::info!(origin = %name, "removed wireguard peer for a deleted origin"),
            Err(e) => tracing::warn!(origin = %name, error = %e, "failed to remove wireguard peer"),
        },
        Err(e) => {
            tracing::error!(origin = %name, error = %e, "cannot remove a peer with an invalid pubkey")
        }
    }
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
                    address = ?reg.tunnel_address,
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
        let Some(Event::Registered(reg)) = parse_sse_event(event) else {
            panic!("expected a registration event");
        };
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
            tunnel_address: None,
        };
        assert!(to_wg_peer(&reg).is_err());
    }

    #[test]
    fn to_wg_peer_leaves_endpoint_unset_when_the_registration_has_none() {
        let reg = PeerRegistration {
            name: "home".into(),
            pubkey: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
            endpoint: None,
            backends: vec![],
            tunnel_address: Some("10.60.0.5".into()),
        };
        let peer = to_wg_peer(&reg).unwrap();
        assert!(peer.endpoint.is_none());
    }

    fn reg(addr: Option<&str>) -> PeerRegistration {
        PeerRegistration {
            name: "home".into(),
            pubkey: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
            endpoint: None,
            backends: vec![],
            tunnel_address: addr.map(str::to_string),
        }
    }

    #[test]
    fn to_wg_peer_routes_the_origins_tunnel_address_as_a_host_route() {
        // Even with no backends yet the origin is reachable at its address.
        let peer = to_wg_peer(&reg(Some("10.60.0.5"))).unwrap();
        assert_eq!(peer.allowed_ips.len(), 1);
        assert_eq!(peer.allowed_ips[0].cidr, 32);
        assert_eq!(peer.allowed_ips[0].address.to_string(), "10.60.0.5");
    }

    #[test]
    fn an_ipv6_origin_is_routed_as_a_slash_128() {
        let peer = to_wg_peer(&reg(Some("fd49::2"))).unwrap();
        assert_eq!(peer.allowed_ips.len(), 1);
        assert_eq!(peer.allowed_ips[0].cidr, 128);
        assert_eq!(peer.allowed_ips[0].address.to_string(), "fd49::2");
    }

    #[test]
    fn an_ipv6_origin_endpoint_is_accepted() {
        let mut r = reg(Some("fd49::2"));
        r.endpoint = Some("[2001:db8::7]:51820".into());
        assert_eq!(
            to_wg_peer(&r).unwrap().endpoint,
            Some("[2001:db8::7]:51820".parse().unwrap())
        );
    }

    #[test]
    fn to_wg_peer_rejects_an_origin_without_a_tunnel_address() {
        assert!(to_wg_peer(&reg(None)).is_err());
    }

    #[test]
    fn parses_a_registration_and_a_tombstone_event() {
        let event = r#"data: {"revision":1,"registration":{"name":"home","pubkey":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=","endpoint":"203.0.113.7:51820","backends":["10.60.0.2:1"],"tunnel_address":"10.60.0.2"}}"#;
        assert!(matches!(parse_sse_event(event), Some(Event::Registered(_))));
        let gone = r#"data: {"revision":2,"removed":{"name":"home"}}"#;
        assert_eq!(parse_sse_event(gone), Some(Event::Removed("home".into())));
    }

    #[test]
    fn plan_skips_unchanged_reconciles_changed_and_removes_known() {
        let mut applied = HashMap::new();
        let r = reg(Some("10.60.0.5"));
        assert_eq!(
            plan(&applied, &Event::Registered(r.clone())),
            Action::Reconcile(&r)
        );
        applied.insert(r.name.clone(), r.clone());
        assert_eq!(plan(&applied, &Event::Registered(r.clone())), Action::Skip);
        assert_eq!(
            plan(&applied, &Event::Removed("home".into())),
            Action::Remove(r.pubkey.clone())
        );
        assert_eq!(
            plan(&applied, &Event::Removed("other".into())),
            Action::Skip
        );
    }

    #[test]
    fn registered_then_removed_leaves_nothing() {
        // A catch-up from revision 0 replays the add and then the removal;
        // applying both in order must end with no peer tracked.
        let mut applied = HashMap::new();
        let r = reg(Some("10.60.0.5"));
        let add = Event::Registered(r.clone());
        match plan(&applied, &add) {
            Action::Reconcile(reg) => {
                applied.insert(reg.name.clone(), reg.clone());
            }
            other => panic!("expected Reconcile, got {other:?}"),
        }
        assert_eq!(applied.len(), 1);
        let del = Event::Removed("home".into());
        assert_eq!(plan(&applied, &del), Action::Remove(r.pubkey.clone()));
        applied.remove("home");
        assert!(applied.is_empty());
    }
    #[test]
    fn a_name_re_registered_under_a_new_key_replaces_the_old_keys_peer() {
        let mut applied = HashMap::new();
        let old = reg(Some("10.60.0.3"));
        assert_eq!(replaced_pubkey(&applied, &old), None, "first sight");
        applied.insert(old.name.clone(), old.clone());
        assert_eq!(replaced_pubkey(&applied, &old), None, "same key");
        let mut new = old.clone();
        new.pubkey = "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB=".into();
        assert_eq!(replaced_pubkey(&applied, &new), Some(old.pubkey.as_str()));
    }
}
