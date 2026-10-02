//! Subscribes to `gsp-controller`'s proxy-peers registry (phase 14 slice 7)
//! and reconciles every registered proxy onto this origin's local WireGuard
//! interface as a peer — the mirror image of `gsp`'s `tunnel_client.rs`,
//! which does the same thing in the other direction (subscribing to the
//! backend-peers registry and reconciling origins). Mirrored rather than
//! shared: `gsp-agent` and `gsp` are independent binaries, same precedent
//! this project already follows for `register.rs`/`gsp::proxy_register`.
//!
//! **Why this exists**: before this, the only way an origin's `gsp-agent`
//! learned about an edge proxy was a static `--peer-pubkey`/`--peer-
//! endpoint` pin at startup — fine for proving the tunnel end to end
//! (phase 14 slice 6), but it means a growing proxy fleet, or a proxy added
//! after this origin was already deployed, needs this agent restarted with
//! new flags. Subscribing here instead means any number of proxies can
//! register with the controller at any time and this origin just picks
//! them up on its next catch-up-then-tail cycle, no restart, matching
//! `docs/11`'s locked topology ("one shared interface with every proxy PoP
//! it's paired with as a peer").
//!
//! `--peer-pubkey`/`--peer-endpoint` still work alongside this — a manual
//! pin converges to the same interface state a registered proxy would
//! reach anyway, so there's no conflict, just redundancy for a bootstrap
//! proxy that predates the registry or a deployment too small to bother
//! with it.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use defguard_wireguard_rs::key::Key;
use defguard_wireguard_rs::peer::Peer;
use defguard_wireguard_rs::WireguardInterfaceApi;
use serde::Deserialize;

const RECONNECT_MIN: Duration = Duration::from_millis(500);
const RECONNECT_MAX: Duration = Duration::from_secs(30);

/// One proxy's registration, exactly as `gsp_controller::proxy_peers::
/// ProxyRegistration` serializes it (duplicated wire shape).
#[derive(Debug, Clone, Deserialize, PartialEq)]
struct ProxyRegistration {
    name: String,
    pubkey: String,
    endpoint: String,
    #[serde(default)]
    tunnel_address: Option<String>,
}

/// Builds the WireGuard peer this registration implies: a **host route to the
/// proxy's own tunnel address** (`/32`) — never `0.0.0.0/0`, which let the
/// last-registered proxy steal every earlier proxy's route — with a keepalive,
/// since a proxy's endpoint is stable but this origin may still be behind NAT.
fn to_wg_peer(reg: &ProxyRegistration) -> anyhow::Result<Peer> {
    let key = Key::try_from(reg.pubkey.as_str())
        .map_err(|e| anyhow::anyhow!("proxy {:?} has an invalid pubkey: {e}", reg.name))?;
    let addr = reg.tunnel_address.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "proxy {:?} has no tunnel_address (is the controller up to date?)",
            reg.name
        )
    })?;
    let ip: std::net::Ipv4Addr = addr.parse().map_err(|e| {
        anyhow::anyhow!(
            "proxy {:?} tunnel_address {addr:?} is invalid: {e}",
            reg.name
        )
    })?;
    let mut peer = Peer::new(key);
    peer.set_allowed_ips(vec![format!("{ip}/32")
        .parse()
        .expect("an IPv4 /32 always parses")]);
    peer.set_endpoint(&reg.endpoint).map_err(|e| {
        anyhow::anyhow!(
            "proxy {:?} endpoint {:?} is invalid: {e}",
            reg.name,
            reg.endpoint
        )
    })?;
    peer.persistent_keepalive_interval = Some(25);
    Ok(peer)
}

#[derive(Debug, PartialEq)]
enum Event {
    Registered(ProxyRegistration),
    Removed(String),
}

/// Parses one SSE event block — identical shape to
/// `gsp::tunnel_client::parse_sse_event`.
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
    Reconcile(&'a ProxyRegistration),
    /// Remove the WireGuard peer with this pubkey.
    Remove(String),
}

/// What to do with `event` given what is already applied — pure, so the
/// catch-up replay (add, then removal) is testable without a WireGuard device.
fn plan<'a>(applied: &HashMap<String, ProxyRegistration>, event: &'a Event) -> Action<'a> {
    match event {
        Event::Registered(reg) if applied.get(&reg.name) == Some(reg) => Action::Skip,
        Event::Registered(reg) => Action::Reconcile(reg),
        Event::Removed(name) => match applied.get(name) {
            Some(old) => Action::Remove(old.pubkey.clone()),
            None => Action::Skip,
        },
    }
}

fn remove_peer(wg: &(dyn WireguardInterfaceApi + Send + Sync), name: &str, pubkey: &str) {
    match Key::try_from(pubkey) {
        Ok(key) => match wg.remove_peer(&key) {
            Ok(()) => tracing::info!(proxy = %name, "removed wireguard peer for a deleted proxy"),
            Err(e) => tracing::warn!(proxy = %name, error = %e, "failed to remove wireguard peer"),
        },
        Err(e) => {
            tracing::error!(proxy = %name, error = %e, "cannot remove a peer with an invalid pubkey")
        }
    }
}

/// Runs forever, reconnecting with backoff on disconnect — identical shape
/// to `gsp::tunnel_client::run`, including the same `last_applied` skip (see
/// that module's doc for why it's required, not just an optimization:
/// `defguard_boringtun`'s userspace backend panics on a same-pubkey
/// `configure_peer`, and re-registering on a fixed interval would otherwise
/// tear down a just-established handshake every cycle).
pub async fn run(
    controller_url: String,
    token: Option<String>,
    wg: Arc<dyn WireguardInterfaceApi + Send + Sync>,
) {
    let mut backoff = RECONNECT_MIN;
    let mut last_applied: HashMap<String, ProxyRegistration> = HashMap::new();
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
                    "proxy-peers subscribe stream ended; reconnecting"
                );
            }
            Err(e) => {
                tracing::warn!(
                    error = %e, controller = %controller_url,
                    "proxy-peers subscribe connection failed; keeping the current \
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
    last_applied: &mut HashMap<String, ProxyRegistration>,
) -> anyhow::Result<()> {
    let url = format!("{base_url}/proxy-peers/subscribe");
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
    tracing::info!(controller = %base_url, "subscribed to proxy-peers updates");

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
            if let Some(ev) = parse_sse_event(&event) {
                match plan(last_applied, &ev) {
                    Action::Skip => {}
                    Action::Reconcile(reg) => {
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

/// Removes then adds — see `gsp::tunnel_client::reconcile_peer`'s doc for
/// why that's required, not defensive, on `defguard_boringtun`'s userspace
/// backend.
fn reconcile_peer(wg: &(dyn WireguardInterfaceApi + Send + Sync), reg: &ProxyRegistration) {
    match to_wg_peer(reg) {
        Ok(peer) => {
            if let Err(e) = wg.remove_peer(&peer.public_key) {
                tracing::debug!(
                    proxy = %reg.name, error = %e,
                    "removing any existing wireguard peer before reconfiguring it \
                     (harmless if it wasn't configured yet)"
                );
            }
            match wg.configure_peer(&peer) {
                Ok(()) => tracing::info!(
                    proxy = %reg.name,
                    endpoint = %reg.endpoint,
                    address = ?reg.tunnel_address,
                    "reconciled wireguard peer for edge proxy"
                ),
                Err(e) => tracing::error!(
                    proxy = %reg.name, error = %e,
                    "failed to configure wireguard peer for edge proxy"
                ),
            }
        }
        Err(e) => tracing::error!(
            proxy = %reg.name, error = %e,
            "skipping a malformed proxy registration"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg(addr: Option<&str>) -> ProxyRegistration {
        ProxyRegistration {
            name: "edge-1".into(),
            pubkey: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
            endpoint: "203.0.113.9:51820".into(),
            tunnel_address: addr.map(str::to_string),
        }
    }

    #[test]
    fn to_wg_peer_routes_only_the_proxys_tunnel_address() {
        let peer = to_wg_peer(&reg(Some("10.60.0.3"))).unwrap();
        assert_eq!(peer.allowed_ips.len(), 1);
        assert_eq!(peer.allowed_ips[0].cidr, 32, "a host route, not 0.0.0.0/0");
        assert_eq!(peer.allowed_ips[0].address.to_string(), "10.60.0.3");
        assert_eq!(peer.persistent_keepalive_interval, Some(25));
        assert!(peer.endpoint.is_some());
    }

    #[test]
    fn to_wg_peer_rejects_a_proxy_without_a_tunnel_address() {
        assert!(to_wg_peer(&reg(None)).is_err());
    }

    #[test]
    fn two_proxies_get_non_overlapping_routes() {
        // The known bug: both used to be 0.0.0.0/0, so the last one won.
        let a = to_wg_peer(&reg(Some("10.60.0.3"))).unwrap();
        let b = to_wg_peer(&reg(Some("10.60.0.4"))).unwrap();
        assert_ne!(a.allowed_ips[0].address, b.allowed_ips[0].address);
    }

    #[test]
    fn parses_a_registration_and_a_tombstone_event() {
        let event = r#"data: {"revision":1,"registration":{"name":"edge-1","pubkey":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=","endpoint":"203.0.113.9:51820","tunnel_address":"10.60.0.3"}}"#;
        match parse_sse_event(event).unwrap() {
            Event::Registered(r) => assert_eq!(r.tunnel_address.as_deref(), Some("10.60.0.3")),
            other => panic!("{other:?}"),
        }
        let gone = r#"data: {"revision":2,"removed":{"name":"edge-1"}}"#;
        assert_eq!(parse_sse_event(gone), Some(Event::Removed("edge-1".into())));
    }

    #[test]
    fn plan_skips_unchanged_reconciles_changed_and_removes_known() {
        let mut applied = HashMap::new();
        let r = reg(Some("10.60.0.3"));
        assert_eq!(
            plan(&applied, &Event::Registered(r.clone())),
            Action::Reconcile(&r)
        );
        applied.insert(r.name.clone(), r.clone());
        assert_eq!(plan(&applied, &Event::Registered(r.clone())), Action::Skip);
        assert_eq!(
            plan(&applied, &Event::Removed("edge-1".into())),
            Action::Remove(r.pubkey.clone())
        );
        assert_eq!(
            plan(&applied, &Event::Removed("other".into())),
            Action::Skip
        );
    }

    #[test]
    fn registered_then_removed_leaves_nothing() {
        // Review Focus 3: a catch-up from revision 0 replays the add and then
        // the removal; applying both in order must end with no peer tracked.
        let mut applied = HashMap::new();
        let r = reg(Some("10.60.0.3"));
        let add = Event::Registered(r.clone());
        if let Action::Reconcile(reg) = plan(&applied, &add) {
            applied.insert(reg.name.clone(), reg.clone());
        }
        let del = Event::Removed("edge-1".into());
        if let Action::Remove(_) = plan(&applied, &del) {
            applied.remove("edge-1");
        }
        assert!(applied.is_empty());
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
        let mut r = reg(Some("10.60.0.3"));
        r.pubkey = "not-a-key".into();
        assert!(to_wg_peer(&r).is_err());
    }

    #[test]
    fn to_wg_peer_rejects_a_malformed_endpoint() {
        let mut r = reg(Some("10.60.0.3"));
        r.endpoint = "not-an-addr".into();
        assert!(to_wg_peer(&r).is_err());
    }
}
