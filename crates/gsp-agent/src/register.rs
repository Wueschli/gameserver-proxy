//! Registers this origin with `gsp-controller`'s backend-peers registry
//! (`POST /peers`, phase 14 slice 2) — the wire shape is duplicated here
//! rather than shared as a library, the same precedent `controller_client`'s
//! hand-parsed SSE JSON and `aggregator_client`'s duplicated `IngestPayload`
//! already established for this codebase's inter-process JSON contracts.

use std::time::Duration;

use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
struct PeerRegistration<'a> {
    name: &'a str,
    pubkey: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    endpoint: Option<&'a str>,
    backends: &'a [String],
}

#[derive(Deserialize)]
struct RegisterResponse {
    revision: u64,
}

/// One registration attempt. Returns the revision the controller assigned.
pub async fn register_once(
    client: &reqwest::Client,
    controller_url: &str,
    token: Option<&str>,
    name: &str,
    pubkey: &str,
    endpoint: Option<&str>,
    backends: &[String],
) -> anyhow::Result<u64> {
    let body = PeerRegistration {
        name,
        pubkey,
        endpoint,
        backends,
    };
    let url = format!("{}/peers", controller_url.trim_end_matches('/'));
    let mut req = client.post(&url).json(&body);
    if let Some(token) = token {
        req = req.bearer_auth(token);
    }
    let resp = req
        .send()
        .await
        .with_context(|| format!("registering with the controller at {url}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("controller rejected registration ({status}): {text}");
    }
    let parsed: RegisterResponse = resp
        .json()
        .await
        .context("parsing the controller's registration response")?;
    Ok(parsed.revision)
}

/// This origin's identity/facts as submitted on every registration —
/// bundled so [`run`]'s signature stays under clippy's argument-count lint
/// instead of taking each field separately.
pub struct Registration {
    pub name: String,
    pub pubkey: String,
    pub endpoint: Option<String>,
    pub backends: Vec<String>,
}

/// Registers immediately, then keeps re-registering every `interval` for as
/// long as the process runs — the "registers on startup and on change"
/// behavior from `docs/08`'s slice-3 plan; since this agent doesn't yet
/// track a live "did anything change" signal, a fixed-interval refresh is
/// the simplest way to keep the controller's `endpoint`/`backends` view from
/// going stale, matching every other control-plane refresh loop in this
/// project's own precedent (`gsp-core::health`'s sweep, `gsp`'s
/// `aggregator_client` push).
pub async fn run(
    client: reqwest::Client,
    controller_url: String,
    token: Option<String>,
    reg: Registration,
    interval: Duration,
) {
    loop {
        match register_once(
            &client,
            &controller_url,
            token.as_deref(),
            &reg.name,
            &reg.pubkey,
            reg.endpoint.as_deref(),
            &reg.backends,
        )
        .await
        {
            Ok(revision) => tracing::info!(revision, "registered with the controller"),
            Err(e) => {
                tracing::warn!(error = %e, "failed to register with the controller; will retry")
            }
        }
        tokio::time::sleep(interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_registration_with_no_endpoint_omits_the_field_entirely() {
        let reg = PeerRegistration {
            name: "home",
            pubkey: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
            endpoint: None,
            backends: &[],
        };
        let json = serde_json::to_string(&reg).unwrap();
        assert!(!json.contains("endpoint"));
    }

    #[test]
    fn a_registration_with_an_endpoint_serializes_it() {
        let backends = vec!["10.60.0.2:1".to_string()];
        let reg = PeerRegistration {
            name: "home",
            pubkey: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
            endpoint: Some("203.0.113.7:51820"),
            backends: &backends,
        };
        let json = serde_json::to_string(&reg).unwrap();
        assert!(json.contains("\"endpoint\":\"203.0.113.7:51820\""));
        assert!(json.contains("\"backends\":[\"10.60.0.2:1\"]"));
    }

    #[tokio::test]
    async fn register_once_surfaces_a_rejection_as_an_error() {
        // No real controller listening on this port — `send()` itself fails
        // (connection refused), which is the common real-world case (the
        // controller not up yet) and should be a clear error, not a panic.
        let client = reqwest::Client::new();
        let result = register_once(
            &client,
            "http://127.0.0.1:1",
            None,
            "home",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
            None,
            &[],
        )
        .await;
        assert!(result.is_err());
    }
}
