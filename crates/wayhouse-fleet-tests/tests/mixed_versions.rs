//! Mixed-version fleets (#185, `docs/upgrading.md`): a node one minor behind
//! keeps working and is sent only baseline fields, and a node on another major
//! is refused readably. The older side is played either by a hand-built request
//! carrying the older header, or by a real binary started with
//! `WAYHOUSE_TEST_PROTOCOL_*` (the `test-protocol-override` feature every fleet
//! build enables, see `PROTOCOL_OVERRIDE_FEATURE`).

use std::time::Duration;

use anyhow::Result;
use serde_json::{json, Value};
use wayhouse_fleet_tests::{
    build_fleet_bins, free_port, spawn_controller_with, wait_http_up, Proc,
};
use wayhouse_http::protocol::{ProtocolVersion, HEADER};

const KEY: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

async fn controller() -> Result<(Proc, tempfile::TempDir, String)> {
    build_fleet_bins()?;
    let dir = tempfile::tempdir()?;
    let port = free_port()?;
    let extra = vec!["--tunnel-network".to_string(), "10.60.0.0/16".to_string()];
    let ctl = spawn_controller_with(dir.path(), &format!("127.0.0.1:{port}"), &extra)?;
    wait_http_up(
        &format!("http://127.0.0.1:{port}/healthz"),
        Duration::from_secs(10),
    )
    .await?;
    Ok((ctl, dir, format!("http://127.0.0.1:{port}")))
}

/// Register a proxy the way a peer announcing `protocol` would.
async fn register(base: &str, name: &str, protocol: &str) -> Result<(u16, Option<String>, Value)> {
    let resp = reqwest::Client::new()
        .post(format!("{base}/proxy-peers"))
        .header(HEADER, protocol)
        .json(&json!({"name": name, "pubkey": KEY, "endpoint": "203.0.113.9:51820"}))
        .send()
        .await?;
    let status = resp.status().as_u16();
    let ours = resp
        .headers()
        .get(HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let text = resp.text().await?;
    let body = serde_json::from_str(&text).unwrap_or(Value::String(text));
    Ok((status, ours, body))
}

#[tokio::test]
async fn proxy_one_minor_behind_registers_and_gets_baseline_fields() -> Result<()> {
    let (_ctl, _dir, base) = controller().await?;
    let current = ProtocolVersion::CURRENT;
    assert!(
        current.minor >= 1,
        "this test plays a peer one minor behind"
    );
    let older = format!("{}.{}", current.major, current.minor - 1);

    let (status, ours, body) = register(&base, "old-proxy", &older).await?;
    assert_eq!(status, 200, "{body}");
    assert_eq!(ours.as_deref(), Some(current.to_string().as_str()));
    assert!(
        body.get("tunnel_address").is_some(),
        "baseline fields stay: {body}"
    );
    let mut keys: Vec<_> = body.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(
        keys,
        ["revision", "tunnel_address", "tunnel_network"],
        "an older peer is sent exactly the baseline fields"
    );

    let (status, _, body) = register(&base, "new-proxy", &current.to_string()).await?;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["capabilities"], json!(["protocol-gating"]));
    Ok(())
}

#[tokio::test]
async fn other_major_peer_is_refused_with_readable_message_and_counter() -> Result<()> {
    let (_ctl, _dir, base) = controller().await?;
    let other = format!("{}.0", ProtocolVersion::CURRENT.major + 1);

    let (status, ours, body) = register(&base, "future-proxy", &other).await?;
    assert_eq!(status, 426);
    assert_eq!(
        ours.as_deref(),
        Some(ProtocolVersion::CURRENT.to_string().as_str())
    );
    let text = body.as_str().unwrap_or_default();
    assert!(
        text.contains(&other) && text.contains("upgrade the older side"),
        "{body}"
    );

    let metrics = wayhouse_http::client()
        .get(format!("{base}/metrics"))
        .send()
        .await?
        .text()
        .await?;
    assert!(
        metrics.contains("wayhouse_protocol_mismatch_total{route_group=\"controller\"} 1"),
        "{metrics}"
    );
    Ok(())
}
