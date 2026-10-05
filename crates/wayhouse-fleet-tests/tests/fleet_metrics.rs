//! `GET /metrics` on the controller, aggregator and UI: gated like the rest of
//! each API, and the TLS handshake counters show up in it.

use std::time::Duration;

use anyhow::{ensure, Result};
use reqwest::{Client, StatusCode};
use tokio::net::TcpSocket;
use wayhouse_fleet_tests::tls_front::{TEST_CA, TEST_LEAF, TEST_LEAF_KEY};
use wayhouse_fleet_tests::{
    build_fleet_bins, free_port, spawn_aggregator_with, spawn_controller_with, spawn_ui_with,
    wait_http_up, wait_until,
};

const TOKEN: &str = "fleet-metrics-test-token";
const SCRAPE_TOKEN: &str = "fleet-scrape-only-token";
const INGEST_TOKEN: &str = "fleet-ingest-only-token";

async fn scrape(client: &Client, url: &str, token: Option<&str>) -> Result<(StatusCode, String)> {
    let mut req = client.get(url);
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let resp = req.send().await?;
    Ok((resp.status(), resp.text().await?))
}

fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(ToString::to_string).collect()
}

#[tokio::test]
async fn controller_and_aggregator_serve_metrics_behind_their_auth_token() -> Result<()> {
    build_fleet_bins()?;
    let dir = tempfile::tempdir()?;
    let (cport, aport) = (free_port()?, free_port()?);
    let _controller = spawn_controller_with(
        dir.path(),
        &format!("127.0.0.1:{cport}"),
        &args(&["--auth-token", TOKEN]),
    )?;
    let _aggregator = spawn_aggregator_with(
        aport,
        &args(&["--auth-token", TOKEN, "--ingest-token", INGEST_TOKEN]),
    )?;
    let client = Client::builder().timeout(Duration::from_secs(10)).build()?;
    for (name, port) in [
        ("wayhouse-controller", cport),
        ("wayhouse-aggregator", aport),
    ] {
        let base = format!("http://127.0.0.1:{port}");
        wait_http_up(&format!("{base}/healthz"), Duration::from_secs(30)).await?;
        let url = format!("{base}/metrics");
        ensure!(
            scrape(&client, &url, None).await?.0 == StatusCode::UNAUTHORIZED,
            "{name}: /metrics open without a token"
        );
        let (status, body) = scrape(&client, &url, Some(TOKEN)).await?;
        ensure!(status.is_success(), "{name}: /metrics {status}");
        ensure!(
            body.contains(&format!(
                "wayhouse_build_info{{component=\"{name}\",version="
            )),
            "{name}: no build info in:\n{body}"
        );
        ensure!(
            body.contains(",commit=\""),
            "{name}: no commit label:\n{body}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn a_metrics_token_unlocks_only_metrics_on_controller_and_aggregator() -> Result<()> {
    build_fleet_bins()?;
    let dir = tempfile::tempdir()?;
    let (cport, aport) = (free_port()?, free_port()?);
    let extra = args(&["--auth-token", TOKEN, "--metrics-token", SCRAPE_TOKEN]);
    let _controller = spawn_controller_with(dir.path(), &format!("127.0.0.1:{cport}"), &extra)?;
    let mut agg_extra = extra.clone();
    agg_extra.extend(args(&["--ingest-token", INGEST_TOKEN]));
    let _aggregator = spawn_aggregator_with(aport, &agg_extra)?;
    let client = Client::builder().timeout(Duration::from_secs(10)).build()?;
    for (name, port, api) in [
        ("wayhouse-controller", cport, "config"),
        ("wayhouse-aggregator", aport, "fleet/pools"),
    ] {
        let base = format!("http://127.0.0.1:{port}");
        wait_http_up(&format!("{base}/healthz"), Duration::from_secs(30)).await?;
        let url = format!("{base}/metrics");
        ensure!(scrape(&client, &url, None).await?.0 == StatusCode::UNAUTHORIZED);
        // The admin token is not the scrape token once one is configured.
        ensure!(
            scrape(&client, &url, Some(TOKEN)).await?.0 == StatusCode::UNAUTHORIZED,
            "{name}: admin token still unlocks /metrics"
        );
        ensure!(
            scrape(&client, &url, Some(SCRAPE_TOKEN))
                .await?
                .0
                .is_success(),
            "{name}: scrape token refused on /metrics"
        );
        // ... and the scrape token opens nothing else.
        let (status, _) = scrape(&client, &format!("{base}/{api}"), Some(SCRAPE_TOKEN)).await?;
        ensure!(
            status == StatusCode::UNAUTHORIZED,
            "{name}: scrape token reached /{api}: {status}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn ui_metrics_need_their_own_token_once_a_login_is_configured() -> Result<()> {
    build_fleet_bins()?;
    let client = Client::builder().timeout(Duration::from_secs(10)).build()?;

    // Open UI (loopback, no login): /metrics is open too.
    let port = free_port()?;
    let _open = spawn_ui_with(port, &[])?;
    let base = format!("http://127.0.0.1:{port}");
    wait_http_up(&format!("{base}/healthz"), Duration::from_secs(30)).await?;
    let (status, body) = scrape(&client, &format!("{base}/metrics"), None).await?;
    ensure!(status.is_success(), "open UI: /metrics {status}");
    ensure!(
        body.contains("wayhouse_build_info{component=\"wayhouse-ui\""),
        "{body}"
    );

    // A login and no --metrics-token: not served at all.
    let port = free_port()?;
    let _login = spawn_ui_with(port, &args(&["--ui-password", "secret"]))?;
    let base = format!("http://127.0.0.1:{port}");
    wait_http_up(&format!("{base}/healthz"), Duration::from_secs(30)).await?;
    let (status, _) = scrape(&client, &format!("{base}/metrics"), None).await?;
    ensure!(
        status != StatusCode::OK,
        "metrics served despite the login and no token"
    );

    // A login and a token: the bearer token (not the session) unlocks it.
    let port = free_port()?;
    let _both = spawn_ui_with(
        port,
        &args(&["--ui-password", "secret", "--metrics-token", TOKEN]),
    )?;
    let url = format!("http://127.0.0.1:{port}/metrics");
    wait_http_up(
        &format!("http://127.0.0.1:{port}/healthz"),
        Duration::from_secs(30),
    )
    .await?;
    ensure!(scrape(&client, &url, None).await?.0 == StatusCode::UNAUTHORIZED);
    ensure!(scrape(&client, &url, Some(TOKEN)).await?.0.is_success());
    Ok(())
}

#[tokio::test]
async fn refused_tls_handshakes_show_up_on_the_aggregators_metrics() -> Result<()> {
    build_fleet_bins()?;
    let port = free_port()?;
    let _aggregator = spawn_aggregator_with(
        port,
        &args(&[
            "--tls-cert",
            TEST_LEAF,
            "--tls-key",
            TEST_LEAF_KEY,
            "--tls-max-pending-per-source",
            "1",
        ]),
    )?;
    let ca = reqwest::Certificate::from_pem(&std::fs::read(TEST_CA)?)?;
    let client = Client::builder()
        .add_root_certificate(ca)
        .timeout(Duration::from_secs(10))
        .build()?;
    let base = format!("https://localhost:{port}");
    wait_until(
        || {
            let (client, url) = (client.clone(), format!("{base}/healthz"));
            async move { Ok(client.get(url).send().await.is_ok()) }
        },
        Duration::from_secs(30),
        "the aggregator to answer over HTTPS",
    )
    .await?;

    // Two idle connects from 127.0.0.2 (the cap is one pending per source); the
    // second is closed at the door. The scrape comes from another source.
    let mut held = Vec::new();
    for _ in 0..2 {
        let socket = TcpSocket::new_v4()?;
        socket.bind("127.0.0.2:0".parse()?)?;
        held.push(socket.connect(([127, 0, 0, 1], port).into()).await?);
    }
    wait_until(
        || {
            let (client, url) = (client.clone(), format!("{base}/metrics"));
            async move {
                let (_, body) = scrape(&client, &url, None).await?;
                Ok(body.contains("wayhouse_tls_handshakes_refused_total{reason=\"per_source\"}"))
            }
        },
        Duration::from_secs(10),
        "a per-source refusal on /metrics",
    )
    .await?;
    drop(held);
    Ok(())
}
