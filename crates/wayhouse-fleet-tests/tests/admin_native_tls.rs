//! `settings.admin.tls`: `wayhouse`'s admin API serves HTTPS itself, reports an
//! `https://` admin URL, and the aggregator's fan-out reaches it there.

use std::time::Duration;

use anyhow::{ensure, Result};
use serde_json::Value;
use tokio::process::Command;
use wayhouse_fleet_tests::tls_front::{TEST_CA, TEST_LEAF, TEST_LEAF_KEY};
use wayhouse_fleet_tests::{
    bin_path, build_fleet_bins, free_port, minimal_wayhouse_config, spawn_aggregator_with,
    spawn_wayhouse, wait_http_up, wait_until, WayhouseArgs,
};

const UP: Duration = Duration::from_secs(30);

/// `minimal_wayhouse_config` with `settings.admin.tls` naming `cert`/`key`.
fn tls_admin_config(admin_port: u16, cert: &str, key: &str) -> Result<String> {
    let base = minimal_wayhouse_config(admin_port, free_port()?, free_port()?);
    let listen = format!("    listen: \"127.0.0.1:{admin_port}\"\n");
    ensure!(
        base.contains(&listen),
        "minimal_wayhouse_config changed shape"
    );
    Ok(base.replace(
        &listen,
        &format!("{listen}    tls:\n      cert: \"{cert}\"\n      key: \"{key}\"\n"),
    ))
}

fn tls_client() -> Result<reqwest::Client> {
    let ca = reqwest::Certificate::from_pem(&std::fs::read(TEST_CA)?)?;
    Ok(wayhouse_http::builder()
        .add_root_certificate(ca)
        .timeout(Duration::from_secs(10))
        .build()?)
}

/// The instance's admin API answers over HTTPS, and a plain-HTTP aggregator
/// (trusting the test CA via `--ca-file`) fans an intent verb out to it through
/// the `https://` admin URL the instance reported.
#[tokio::test]
async fn the_aggregator_fans_out_to_an_https_admin_api() -> Result<()> {
    build_fleet_bins()?;
    let agg_port = free_port()?;
    let agg_url = format!("http://127.0.0.1:{agg_port}");
    let _aggregator = spawn_aggregator_with(agg_port, &["--ca-file", TEST_CA].map(String::from))?;
    wait_http_up(&format!("{agg_url}/healthz"), UP).await?;

    let admin_port = free_port()?;
    let config = tempfile::NamedTempFile::new()?;
    std::fs::write(
        config.path(),
        tls_admin_config(admin_port, TEST_LEAF, TEST_LEAF_KEY)?,
    )?;
    let _wayhouse = spawn_wayhouse(WayhouseArgs {
        config_path: Some(config.path().to_path_buf()),
        aggregator_url: Some(agg_url.clone()),
        aggregator_instance: Some("tls-admin".to_string()),
        aggregator_interval_sec: 1,
        ..Default::default()
    })?;

    let client = tls_client()?;
    let admin = format!("https://127.0.0.1:{admin_port}");
    wait_until(
        || {
            let (client, url) = (client.clone(), format!("{admin}/healthz"));
            async move { Ok(client.get(url).send().await.is_ok()) }
        },
        UP,
        "the admin API to answer over HTTPS",
    )
    .await?;
    ensure!(
        reqwest::get(format!("http://127.0.0.1:{admin_port}/healthz"))
            .await
            .is_err(),
        "the admin API still answered plain HTTP"
    );

    wait_until(
        || {
            let url = format!("{agg_url}/fleet/pools");
            async move {
                let Ok(resp) = reqwest::get(url).await else {
                    return Ok(false);
                };
                let body: Value = resp.json().await.unwrap_or_default();
                Ok(body
                    .as_array()
                    .is_some_and(|all| all.iter().any(|i| i["instance"] == "tls-admin")))
            }
        },
        UP,
        "the aggregator to ingest the instance",
    )
    .await?;

    let body: Value = wayhouse_http::client()
        .post(format!("{agg_url}/fleet/pools/local/backends"))
        .json(&serde_json::json!({ "addr": "127.0.0.1:1" }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let results = body["results"].as_array().cloned().unwrap_or_default();
    ensure!(
        results.len() == 1 && results[0]["status"] == 200,
        "fan-out to the https admin API: {body}"
    );
    Ok(())
}

/// `wayhouse --check` loads the pair, and an error names the setting and the file.
#[tokio::test]
async fn check_names_a_bad_admin_certificate() -> Result<()> {
    build_fleet_bins()?;
    let config = tempfile::NamedTempFile::new()?;
    std::fs::write(
        config.path(),
        tls_admin_config(free_port()?, "/nonexistent/cert.pem", TEST_LEAF_KEY)?,
    )?;
    let out = tokio::time::timeout(
        Duration::from_secs(30),
        Command::new(bin_path("wayhouse"))
            .args(["--check", "--config", &config.path().display().to_string()])
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    ensure!(!out.status.success(), "--check passed:\n{text}");
    ensure!(
        text.contains("settings.admin.tls.cert /nonexistent/cert.pem"),
        "did not name the setting and file:\n{text}"
    );
    Ok(())
}
