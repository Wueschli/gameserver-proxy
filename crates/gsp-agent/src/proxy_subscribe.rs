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
#[derive(Debug, Deserialize, PartialEq)]
struct ProxyRegistration {
    name: String,
    pubkey: String,
    endpoint: String,
}

/// Builds the WireGuard peer this registration implies: a full-tunnel
/// route (this origin has exactly one proxy-side "gateway" concern per
/// peer — its own backends live behind its own interface's `AllowedIPs` on
/// the proxy's side, not the other way round) with a keepalive, since a
/// proxy's endpoint is stable but this origin may still be behind NAT.
fn to_wg_peer(reg: &ProxyRegistration) -> anyhow::Result<Peer> {
    let key = Key::try_from(reg.pubkey.as_str())
        .map_err(|e| anyhow::anyhow!("proxy {:?} has an invalid pubkey: {e}", reg.name))?;
    let mut peer = Peer::new(key);
    // `0.0.0.0/0` always parses — same literal `main.rs`'s own
    // `--peer-pubkey`/`--peer-endpoint` peer already uses.
    peer.set_allowed_ips(vec!["0.0.0.0/0".parse().unwrap()]);
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

/// Parses one SSE event block — identical shape to
/// `gsp::tunnel_client::parse_sse_event`.
fn parse_sse_event(event: &str) -> Option<ProxyRegistration> {
    let data_line = event
        .split('\n')
        .find_map(|line| line.strip_prefix("data:"))?
        .trim_start();
    let payload: serde_json::Value = serde_json::from_str(data_line).ok()?;
    serde_json::from_value(payload.get("registration")?.clone()).ok()
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

    #[test]
    fn parses_a_well_formed_data_event() {
        let event = r#"data: {"revision":1,"registration":{"name":"edge-1","pubkey":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=","endpoint":"203.0.113.9:51820"}}"#;
        let reg = parse_sse_event(event).unwrap();
        assert_eq!(reg.name, "edge-1");
        assert_eq!(reg.endpoint, "203.0.113.9:51820");
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
        let reg = ProxyRegistration {
            name: "edge-1".into(),
            pubkey: "not-a-key".into(),
            endpoint: "203.0.113.9:51820".into(),
        };
        assert!(to_wg_peer(&reg).is_err());
    }

    #[test]
    fn to_wg_peer_rejects_a_malformed_endpoint() {
        let reg = ProxyRegistration {
            name: "edge-1".into(),
            pubkey: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
            endpoint: "not-an-addr".into(),
        };
        assert!(to_wg_peer(&reg).is_err());
    }

    #[test]
    fn to_wg_peer_builds_a_full_tunnel_route_with_keepalive() {
        let reg = ProxyRegistration {
            name: "edge-1".into(),
            pubkey: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
            endpoint: "203.0.113.9:51820".into(),
        };
        let peer = to_wg_peer(&reg).unwrap();
        assert_eq!(peer.allowed_ips.len(), 1);
        assert_eq!(peer.persistent_keepalive_interval, Some(25));
        assert!(peer.endpoint.is_some());
    }
}
