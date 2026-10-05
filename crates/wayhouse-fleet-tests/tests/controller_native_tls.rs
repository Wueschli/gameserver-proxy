//! `wayhouse-controller --tls-cert/--tls-key`: the controller serves HTTPS itself.

use std::time::Duration;

use anyhow::{ensure, Result};
use tokio::process::Command;
use wayhouse_fleet_tests::tls_front::{TEST_CA, TEST_LEAF, TEST_LEAF_KEY};
use wayhouse_fleet_tests::{
    bin_path, build_fleet_bins, free_port, minimal_wayhouse_config, spawn_controller_with,
    wait_until, Proc,
};

struct Tls {
    base: String,
    client: reqwest::Client,
    _proc: Proc,
    _dir: tempfile::TempDir,
}

/// A controller serving native TLS with the test leaf, up and holding one config.
async fn tls_controller() -> Result<Tls> {
    build_fleet_bins()?;
    let dir = tempfile::tempdir()?;
    let port = free_port()?;
    let proc = spawn_controller_with(
        dir.path(),
        &format!("127.0.0.1:{port}"),
        &["--tls-cert", TEST_LEAF, "--tls-key", TEST_LEAF_KEY].map(String::from),
    )?;
    let ca = reqwest::Certificate::from_pem(&std::fs::read(TEST_CA)?)?;
    let client = reqwest::Client::builder()
        .add_root_certificate(ca)
        .timeout(Duration::from_secs(10))
        .build()?;
    let base = format!("https://localhost:{port}");
    wait_until(
        || {
            let (client, base) = (client.clone(), base.clone());
            async move { Ok(client.get(format!("{base}/healthz")).send().await.is_ok()) }
        },
        Duration::from_secs(30),
        "the controller to answer over HTTPS",
    )
    .await?;
    client
        .post(format!("{base}/config"))
        .body(minimal_wayhouse_config(
            free_port()?,
            free_port()?,
            free_port()?,
        ))
        .send()
        .await?
        .error_for_status()?;
    Ok(Tls {
        base,
        client,
        _proc: proc,
        _dir: dir,
    })
}

async fn wayhouse_check(base: &str, extra: &[&str]) -> Result<(bool, String)> {
    let out = tokio::time::timeout(
        Duration::from_secs(60),
        Command::new(bin_path("wayhouse"))
            .args(["--check", "--controller", base])
            .args(extra)
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    Ok((out.status.success(), text))
}

#[tokio::test]
async fn wayhouse_check_reaches_a_natively_tls_controller() -> Result<()> {
    let c = tls_controller().await?;
    let (ok, text) = wayhouse_check(&c.base, &[]).await?;
    ensure!(!ok, "trusted a private CA without --ca-file:\n{text}");
    ensure!(
        text.contains("UnknownIssuer"),
        "failed, but not on certificate verification:\n{text}"
    );
    let (ok, text) = wayhouse_check(&c.base, &["--ca-file", TEST_CA]).await?;
    ensure!(ok, "wayhouse --check failed with --ca-file:\n{text}");
    Ok(())
}

#[tokio::test]
async fn config_subscribe_streams_over_native_tls() -> Result<()> {
    let c = tls_controller().await?;
    let mut resp = c
        .client
        .get(format!("{}/config/subscribe", c.base))
        .timeout(Duration::from_secs(60))
        .send()
        .await?
        .error_for_status()?;
    let chunk = tokio::time::timeout(Duration::from_secs(10), resp.chunk()).await??;
    let text = String::from_utf8_lossy(&chunk.unwrap_or_default()).to_string();
    ensure!(
        text.contains("data:") || text.contains("event:"),
        "first SSE chunk: {text:?}"
    );
    Ok(())
}

#[tokio::test]
async fn tls_cert_without_key_is_refused() -> Result<()> {
    build_fleet_bins()?;
    let dir = tempfile::tempdir()?;
    let out = tokio::time::timeout(
        Duration::from_secs(30),
        Command::new(bin_path("wayhouse-controller"))
            .args(["--data-dir", &dir.path().display().to_string()])
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
async fn a_zero_handshake_cap_is_refused() -> Result<()> {
    build_fleet_bins()?;
    let dir = tempfile::tempdir()?;
    let out = tokio::time::timeout(
        Duration::from_secs(30),
        Command::new(bin_path("wayhouse-controller"))
            .args(["--data-dir", &dir.path().display().to_string()])
            .args(["--listen", &format!("127.0.0.1:{}", free_port()?)])
            .args(["--tls-max-pending", "0"])
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    let text = String::from_utf8_lossy(&out.stderr);
    ensure!(!out.status.success(), "started with --tls-max-pending 0");
    ensure!(text.contains("--tls-max-pending"), "{text}");
    Ok(())
}
