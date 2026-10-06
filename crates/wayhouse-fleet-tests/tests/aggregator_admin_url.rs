//! `wayhouse --aggregator-admin-url`: the instance reports the given URL instead of
//! `http(s)://<settings.admin.listen>`, so the aggregator's fan-out reaches an
//! admin API that sits behind something else — the container/k8s shape, here
//! a TLS terminator on another port in front of a plain-HTTP admin API.

use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{ensure, Result};
use serde_json::Value;
use wayhouse_fleet_tests::tls_front::{tls_front_counted, TEST_CA};
use wayhouse_fleet_tests::{
    build_fleet_bins, free_port, minimal_wayhouse_config, spawn_aggregator_with, spawn_wayhouse,
    wait_http_up, wait_until, WayhouseArgs,
};

const UP: Duration = Duration::from_secs(30);

/// The fan-out goes to the overridden URL (through the terminator, which
/// counts it), not to the admin listener the instance would otherwise report.
#[tokio::test]
async fn the_aggregator_fans_out_through_the_overridden_admin_url() -> Result<()> {
    build_fleet_bins()?;
    let agg_port = free_port()?;
    let agg_url = format!("http://127.0.0.1:{agg_port}");
    // The reported host (`localhost`) is not the pusher's source IP, so the
    // aggregator needs it on its `--instance-url-allow` list.
    let _aggregator = spawn_aggregator_with(
        agg_port,
        &["--ca-file", TEST_CA, "--instance-url-allow", "localhost"].map(String::from),
    )?;
    wait_http_up(&format!("{agg_url}/healthz"), UP).await?;

    let admin_port = free_port()?;
    let config = tempfile::NamedTempFile::new()?;
    std::fs::write(
        config.path(),
        minimal_wayhouse_config(admin_port, free_port()?, free_port()?),
    )?;
    let (front, _task, accepted) =
        tls_front_counted(format!("127.0.0.1:{admin_port}").parse()?).await?;
    let _wayhouse = spawn_wayhouse(WayhouseArgs {
        config_path: Some(config.path().to_path_buf()),
        aggregator_url: Some(agg_url.clone()),
        aggregator_instance: Some("behind-front".to_string()),
        aggregator_interval_sec: 1,
        // A trailing slash is trimmed, so the fan-out's paths don't double it.
        extra: vec![
            "--aggregator-admin-url".to_string(),
            format!("https://localhost:{}/", front.port()),
        ],
        ..Default::default()
    })?;
    wait_http_up(&format!("http://127.0.0.1:{admin_port}/healthz"), UP).await?;

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
                    .is_some_and(|all| all.iter().any(|i| i["instance"] == "behind-front")))
            }
        },
        UP,
        "the aggregator to ingest the instance",
    )
    .await?;

    ensure!(
        accepted.load(Ordering::Relaxed) == 0,
        "nothing dialled the front yet"
    );
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
        "fan-out through the overridden admin URL: {body}"
    );
    ensure!(
        accepted.load(Ordering::Relaxed) >= 1,
        "the fan-out bypassed the overridden admin URL"
    );
    Ok(())
}
