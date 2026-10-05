//! `wayhouse-aggregator --ingest-token` and the `admin_url` trust rule, against the
//! real binary (so the pusher's source address is a real socket peer).

use std::time::Duration;

use anyhow::{ensure, Result};
use serde_json::{json, Value};
use wayhouse_fleet_tests::{build_fleet_bins, free_port, spawn_aggregator_with, wait_http_up};

const UP: Duration = Duration::from_secs(30);
const ADMIN: &str = "admin-token-0123456789";
const INGEST: &str = "ingest-token-0123456789";

fn push(url: &str, token: &str, admin_url: &str) -> reqwest::RequestBuilder {
    reqwest::Client::new()
        .post(format!("{url}/ingest"))
        .bearer_auth(token)
        .json(&json!({ "instance": "edge-1", "admin_url": admin_url, "pools": [] }))
}

#[tokio::test]
async fn the_ingest_token_only_pushes_and_admin_urls_must_be_the_pushers_own() -> Result<()> {
    build_fleet_bins()?;
    let port = free_port()?;
    let url = format!("http://127.0.0.1:{port}");
    let _aggregator = spawn_aggregator_with(
        port,
        &["--auth-token", ADMIN, "--ingest-token", INGEST].map(String::from),
    )?;
    wait_http_up(&format!("{url}/healthz"), UP).await?;

    // The admin token no longer pushes; the ingest token does.
    let own = "http://127.0.0.1:9900";
    ensure!(push(&url, ADMIN, own).send().await?.status() == 401);
    // A pusher naming a host it controls is refused, so the fan-out never
    // sends the instance token there.
    let resp = push(&url, INGEST, "http://evil.example:9900")
        .send()
        .await?;
    ensure!(resp.status() == 400, "evil admin_url: {}", resp.status());
    let resp = push(&url, INGEST, own).send().await?;
    ensure!(resp.status() == 200, "own admin_url: {}", resp.status());

    // The ingest token cannot read or drive the fleet.
    let client = reqwest::Client::new();
    let get = |token: &str| client.get(format!("{url}/fleet/pools")).bearer_auth(token);
    ensure!(get(INGEST).send().await?.status() == 401);
    let body: Value = get(ADMIN).send().await?.error_for_status()?.json().await?;
    ensure!(body[0]["instance"] == "edge-1", "{body}");
    let drain = client
        .post(format!("{url}/fleet/instances/edge-1/drain"))
        .bearer_auth(INGEST)
        .send()
        .await?;
    ensure!(drain.status() == 401);
    Ok(())
}
