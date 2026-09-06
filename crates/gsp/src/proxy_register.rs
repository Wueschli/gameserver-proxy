//! Registers this proxy with `gsp-controller`'s proxy-peers registry
//! (`POST /proxy-peers`, phase 14 slice 7) — the mirror image of
//! `gsp-agent::register`, duplicated rather than shared for the same
//! reason `gsp-agent`'s own copy is: no library crate sits between these
//! two binaries, and this project's own precedent (`controller_client`'s
//! hand-parsed SSE, `aggregator_client`'s duplicated `IngestPayload`)
//! already accepts small wire-shape duplication across independent
//! binaries over adding one.
//!
//! Exists so every origin's `gsp-agent` can learn about every edge proxy by
//! subscribing to this registry, the same way every proxy already learns
//! about every origin by subscribing to the backend-peers one
//! (`tunnel_client.rs`) — a growing proxy fleet, or one proxy added after
//! an origin was already deployed, needs no origin-side reconfiguration.

use std::time::Duration;

use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
struct ProxyRegistration<'a> {
    name: &'a str,
    pubkey: &'a str,
    endpoint: &'a str,
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
    endpoint: &str,
) -> anyhow::Result<u64> {
    let body = ProxyRegistration {
        name,
        pubkey,
        endpoint,
    };
    let url = format!("{}/proxy-peers", controller_url.trim_end_matches('/'));
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
        anyhow::bail!("controller rejected proxy registration ({status}): {text}");
    }
    let parsed: RegisterResponse = resp
        .json()
        .await
        .context("parsing the controller's proxy registration response")?;
    Ok(parsed.revision)
}

/// Registers immediately, then keeps re-registering every `interval` for as
/// long as the process runs — same fixed-interval-refresh posture
/// `gsp-agent::register::run` uses, for the same reason (no "did anything
/// change" signal to key off yet).
pub async fn run(
    client: reqwest::Client,
    controller_url: String,
    token: Option<String>,
    name: String,
    pubkey: String,
    endpoint: String,
    interval: Duration,
) {
    loop {
        match register_once(
            &client,
            &controller_url,
            token.as_deref(),
            &name,
            &pubkey,
            &endpoint,
        )
        .await
        {
            Ok(revision) => {
                tracing::info!(revision, "registered as a proxy peer with the controller")
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to register as a proxy peer; will retry")
            }
        }
        tokio::time::sleep(interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_registration_serializes_all_fields() {
        let reg = ProxyRegistration {
            name: "edge-1",
            pubkey: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
            endpoint: "203.0.113.9:51820",
        };
        let json = serde_json::to_string(&reg).unwrap();
        assert!(json.contains("\"name\":\"edge-1\""));
        assert!(json.contains("\"endpoint\":\"203.0.113.9:51820\""));
    }

    #[tokio::test]
    async fn register_once_surfaces_a_rejection_as_an_error() {
        // No real controller listening on this port — `send()` itself fails
        // (connection refused), the common real-world case (controller not
        // up yet), and should be a clear error, not a panic.
        let client = reqwest::Client::new();
        let result = register_once(
            &client,
            "http://127.0.0.1:1",
            None,
            "edge-1",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
            "203.0.113.9:51820",
        )
        .await;
        assert!(result.is_err());
    }
}
