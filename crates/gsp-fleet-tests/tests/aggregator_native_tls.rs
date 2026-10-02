//! `gsp-aggregator --tls-cert/--tls-key`: the aggregator serves HTTPS itself.

use std::time::Duration;

use anyhow::{ensure, Result};
use gsp_fleet_tests::tls_front::{TEST_CA, TEST_LEAF, TEST_LEAF_KEY};
use gsp_fleet_tests::{
    bin_path, build_fleet_bins, free_port, minimal_gsp_config, spawn_aggregator_with, spawn_gsp,
    wait_http_up, wait_until, GspArgs,
};
use serde_json::Value;
use tokio::process::Command;

const UP: Duration = Duration::from_secs(30);

fn tls_client() -> Result<reqwest::Client> {
    let ca = reqwest::Certificate::from_pem(&std::fs::read(TEST_CA)?)?;
    Ok(reqwest::Client::builder()
        .add_root_certificate(ca)
        .timeout(Duration::from_secs(10))
        .build()?)
}

/// A `gsp` pushing to an `https://` aggregator (trusting the test CA through
/// `--ca-file`) shows up in that aggregator's `/fleet/pools`, read over HTTPS.
#[tokio::test]
async fn gsp_pushes_to_a_natively_tls_aggregator() -> Result<()> {
    build_fleet_bins()?;
    let agg_port = free_port()?;
    let _aggregator = spawn_aggregator_with(
        agg_port,
        &["--tls-cert", TEST_LEAF, "--tls-key", TEST_LEAF_KEY].map(String::from),
    )?;
    let agg_url = format!("https://localhost:{agg_port}");
    let client = tls_client()?;
    wait_until(
        || {
            let (client, url) = (client.clone(), format!("{agg_url}/healthz"));
            async move { Ok(client.get(url).send().await.is_ok()) }
        },
        UP,
        "the aggregator to answer over HTTPS",
    )
    .await?;

    let admin_port = free_port()?;
    let config = tempfile::NamedTempFile::new()?;
    std::fs::write(
        config.path(),
        minimal_gsp_config(admin_port, free_port()?, free_port()?),
    )?;
    let _gsp = spawn_gsp(GspArgs {
        config_path: Some(config.path().to_path_buf()),
        aggregator_url: Some(agg_url.clone()),
        aggregator_instance: Some("tls-instance".to_string()),
        aggregator_interval_sec: 1,
        extra: ["--ca-file", TEST_CA].map(String::from).to_vec(),
        ..Default::default()
    })?;
    wait_http_up(&format!("http://127.0.0.1:{admin_port}/healthz"), UP).await?;

    wait_until(
        || {
            let (client, url) = (client.clone(), format!("{agg_url}/fleet/pools"));
            async move {
                let body: Value = client.get(url).send().await?.json().await?;
                Ok(body
                    .as_array()
                    .is_some_and(|all| all.iter().any(|i| i["instance"] == "tls-instance")))
            }
        },
        UP,
        "the aggregator to ingest a push over HTTPS",
    )
    .await
}

#[tokio::test]
async fn aggregator_tls_cert_without_key_is_refused() -> Result<()> {
    build_fleet_bins()?;
    let out = tokio::time::timeout(
        Duration::from_secs(30),
        Command::new(bin_path("gsp-aggregator"))
            .args(["--listen", &format!("127.0.0.1:{}", free_port()?)])
            .args(["--tls-cert", TEST_LEAF])
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    let text = String::from_utf8_lossy(&out.stderr);
    ensure!(!out.status.success(), "started with --tls-cert alone");
    ensure!(text.contains("--tls-key"), "{text}");
    Ok(())
}

#[tokio::test]
async fn aggregator_names_a_bad_certificate_file() -> Result<()> {
    build_fleet_bins()?;
    let out = tokio::time::timeout(
        Duration::from_secs(30),
        Command::new(bin_path("gsp-aggregator"))
            .args(["--listen", &format!("127.0.0.1:{}", free_port()?)])
            .args([
                "--tls-cert",
                "/nonexistent/cert.pem",
                "--tls-key",
                TEST_LEAF_KEY,
            ])
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    let text = String::from_utf8_lossy(&out.stderr);
    ensure!(!out.status.success(), "started with a missing certificate");
    ensure!(
        text.contains("--tls-cert /nonexistent/cert.pem"),
        "did not name the file:\n{text}"
    );
    Ok(())
}
