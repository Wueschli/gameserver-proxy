//! Slice 12 integration tests: multiple real fleet binaries, spawned as real
//! processes, talking over real loopback sockets. See `src/lib.rs` for why
//! this crate exists and why it deliberately isn't an in-process harness.
//!
//! Each test builds the fleet binaries once per test-binary run (`OnceLock`
//! below — cargo's own target-dir locking makes a second concurrent build
//! safe, this just avoids redundant `cargo build` invocations) and cleans up
//! every spawned `Proc` via `Drop`/`kill_on_drop`, so a failing assertion
//! never leaves a stray process squatting on a port for the rest of the
//! suite.

use std::sync::OnceLock;
use std::time::Duration;

use anyhow::Result;
use serde_json::Value;
use wayhouse_fleet_tests::*;

fn ensure_built() {
    static BUILT: OnceLock<()> = OnceLock::new();
    BUILT.get_or_init(|| build_fleet_bins().expect("building fleet binaries"));
}

const UP: Duration = Duration::from_secs(10);

/// Controller subscribe/reconnect/freeze-on-disconnect: `wayhouse --controller`
/// picks up the controller's initial config, keeps running on it when the
/// controller disappears (never crashes, never falls back to some default),
/// then picks up a new revision once the controller (same data dir, so the
/// revision history survived) comes back.
#[tokio::test]
async fn controller_reconnect_freezes_then_catches_up() -> Result<()> {
    ensure_built();

    let data_dir = tempfile::tempdir()?;
    let controller_port = free_port()?;
    let controller_url = format!("http://127.0.0.1:{controller_port}");

    let controller = spawn_controller(data_dir.path(), controller_port)?;
    wait_http_up(&format!("{controller_url}/healthz"), UP).await?;

    let wayhouse_admin_port = free_port()?;
    let wayhouse_listen_port_a = free_port()?;
    let target_port = free_port()?;
    reqwest::Client::new()
        .post(format!("{controller_url}/config"))
        .body(minimal_wayhouse_config(
            wayhouse_admin_port,
            wayhouse_listen_port_a,
            target_port,
        ))
        .send()
        .await?
        .error_for_status()?;

    let wayhouse = spawn_wayhouse(WayhouseArgs {
        controller_url: Some(controller_url.clone()),
        ..Default::default()
    })?;
    let wayhouse_admin = format!("http://127.0.0.1:{wayhouse_admin_port}");
    wait_http_up(&format!("{wayhouse_admin}/healthz"), UP).await?;

    // It pulled revision 1: its own /config reflects that listener's bind.
    wait_until(
        || {
            let wayhouse_admin = wayhouse_admin.clone();
            async move {
                let body = reqwest::get(format!("{wayhouse_admin}/config"))
                    .await?
                    .text()
                    .await?;
                Ok(body.contains(&format!(":{wayhouse_listen_port_a}")))
            }
        },
        UP,
        "wayhouse to pull revision 1 from the controller",
    )
    .await?;

    // Kill the controller; wayhouse must keep serving its last-known config.
    controller.kill().await?;
    assert!(
        port_is_down(controller_port, UP).await,
        "controller's port should be freed after kill"
    );
    tokio::time::sleep(Duration::from_secs(2)).await;
    let body = reqwest::get(format!("{wayhouse_admin}/config"))
        .await?
        .text()
        .await?;
    assert!(
        body.contains(&format!(":{wayhouse_listen_port_a}")),
        "wayhouse should have frozen on its last config, not reset: {body}"
    );

    // Bring the controller back on the same data dir (same revision
    // history) and push a second revision; wayhouse's reconnect loop should
    // catch up without a restart.
    let controller2 = spawn_controller(data_dir.path(), controller_port)?;
    wait_http_up(&format!("{controller_url}/healthz"), UP).await?;
    let wayhouse_listen_port_b = free_port()?;
    reqwest::Client::new()
        .post(format!("{controller_url}/config"))
        .body(minimal_wayhouse_config(
            wayhouse_admin_port,
            wayhouse_listen_port_b,
            target_port,
        ))
        .send()
        .await?
        .error_for_status()?;

    wait_until(
        || {
            let wayhouse_admin = wayhouse_admin.clone();
            async move {
                let body = reqwest::get(format!("{wayhouse_admin}/config"))
                    .await?
                    .text()
                    .await?;
                Ok(body.contains(&format!(":{wayhouse_listen_port_b}")))
            }
        },
        UP,
        "wayhouse to reconnect and pull revision 2",
    )
    .await?;

    drop(wayhouse);
    drop(controller2);
    Ok(())
}

/// Controller: a submission that fails `wayhouse_config::validate()` never
/// displaces the current revision — same "bad reload keeps the old
/// snapshot" rule a file-based proxy reload already has, one hop earlier.
#[tokio::test]
async fn controller_rejects_bad_config_and_keeps_previous() -> Result<()> {
    ensure_built();

    let data_dir = tempfile::tempdir()?;
    let controller_port = free_port()?;
    let base = format!("http://127.0.0.1:{controller_port}");
    let controller = spawn_controller(data_dir.path(), controller_port)?;
    wait_http_up(&format!("{base}/healthz"), UP).await?;

    let client = reqwest::Client::new();
    let good = minimal_wayhouse_config(free_port()?, free_port()?, free_port()?);
    let resp = client
        .post(format!("{base}/config"))
        .body(good.clone())
        .send()
        .await?;
    assert_eq!(resp.status(), 200);
    let submitted: Value = resp.json().await?;
    assert_eq!(submitted["revision"], 1);

    let resp = client
        .post(format!("{base}/config"))
        .body(invalid_wayhouse_config())
        .send()
        .await?;
    assert_eq!(
        resp.status(),
        422,
        "a schema-invalid submission must be rejected, not accepted"
    );

    let resp = client.get(format!("{base}/config")).send().await?;
    assert_eq!(
        resp.headers().get("X-Config-Revision").unwrap(),
        "1",
        "current revision must be unchanged by the rejected submission"
    );
    let body = resp.text().await?;
    assert_eq!(body, good, "current config body must be the last good one");

    drop(controller);
    Ok(())
}

/// Aggregator push/ingest: a real `wayhouse --aggregator` instance's periodic
/// push actually lands and shows up in `GET /fleet/pools`.
#[tokio::test]
async fn wayhouse_pushes_state_that_the_aggregator_ingests() -> Result<()> {
    ensure_built();

    let agg_port = free_port()?;
    let agg_url = format!("http://127.0.0.1:{agg_port}");
    let aggregator = spawn_aggregator(agg_port)?;
    wait_http_up(&format!("{agg_url}/healthz"), UP).await?;

    let admin_port = free_port()?;
    let listen_port = free_port()?;
    let target_port = free_port()?;
    let config = tempfile::NamedTempFile::new()?;
    std::fs::write(
        config.path(),
        minimal_wayhouse_config(admin_port, listen_port, target_port),
    )?;

    let wayhouse = spawn_wayhouse(WayhouseArgs {
        config_path: Some(config.path().to_path_buf()),
        aggregator_url: Some(agg_url.clone()),
        aggregator_instance: Some("fleet-test-instance".to_string()),
        aggregator_interval_sec: 1,
        ..Default::default()
    })?;
    wait_http_up(&format!("http://127.0.0.1:{admin_port}/healthz"), UP).await?;

    wait_until(
        || {
            let agg_url = agg_url.clone();
            async move {
                let body: Value = reqwest::get(format!("{agg_url}/fleet/pools"))
                    .await?
                    .json()
                    .await?;
                Ok(body.as_array().is_some_and(|instances| {
                    instances
                        .iter()
                        .any(|i| i["instance"] == "fleet-test-instance")
                }))
            }
        },
        UP,
        "the aggregator to ingest the pushed instance state",
    )
    .await?;

    drop(wayhouse);
    drop(aggregator);
    Ok(())
}

/// Fan-out partial failure: broadcasting a backend-add across two known
/// instances, one of which has since died, must report a per-instance
/// result for each — success for the live one, "unreachable" for the dead
/// one — never fail (or hang) the whole request.
#[tokio::test]
async fn fanout_broadcast_partial_failure() -> Result<()> {
    ensure_built();

    let agg_port = free_port()?;
    let agg_url = format!("http://127.0.0.1:{agg_port}");
    let aggregator = spawn_aggregator(agg_port)?;
    wait_http_up(&format!("{agg_url}/healthz"), UP).await?;

    let mut configs = Vec::new();
    let mut admin_ports = Vec::new();
    for _ in 0..2 {
        let admin_port = free_port()?;
        let listen_port = free_port()?;
        let target_port = free_port()?;
        let config = tempfile::NamedTempFile::new()?;
        std::fs::write(
            config.path(),
            minimal_wayhouse_config(admin_port, listen_port, target_port),
        )?;
        admin_ports.push(admin_port);
        configs.push(config);
    }

    let mut instances = Vec::new();
    for (i, config) in configs.iter().enumerate() {
        instances.push(spawn_wayhouse(WayhouseArgs {
            config_path: Some(config.path().to_path_buf()),
            aggregator_url: Some(agg_url.clone()),
            aggregator_instance: Some(format!("fanout-{i}")),
            aggregator_interval_sec: 1,
            ..Default::default()
        })?);
    }
    for port in &admin_ports {
        wait_http_up(&format!("http://127.0.0.1:{port}/healthz"), UP).await?;
    }

    wait_until(
        || {
            let agg_url = agg_url.clone();
            async move {
                let body: Value = reqwest::get(format!("{agg_url}/fleet/pools"))
                    .await?
                    .json()
                    .await?;
                let names: Vec<&str> = body
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|i| i["instance"].as_str())
                    .collect();
                Ok(names.contains(&"fanout-0") && names.contains(&"fanout-1"))
            }
        },
        UP,
        "the aggregator to ingest both instances before killing one",
    )
    .await?;

    // Kill instance 1 and make sure its admin port is actually gone before
    // fanning out — otherwise the broadcast could race a still-listening
    // (but about-to-die) socket and flake between the two outcomes.
    let dead_port = admin_ports[1];
    instances.pop().unwrap().kill().await?;
    assert!(
        port_is_down(dead_port, UP).await,
        "instance 1's admin port should be gone after kill"
    );

    let resp = reqwest::Client::new()
        .post(format!("{agg_url}/fleet/pools/local/backends"))
        .json(&serde_json::json!({ "addr": "127.0.0.1:1" }))
        .send()
        .await?;
    assert_eq!(resp.status(), 200, "the broadcast call itself must succeed");
    let body: Value = resp.json().await?;
    let results = body["results"].as_array().expect("results array");
    assert_eq!(results.len(), 2, "one result per known instance");
    let ok_count = results.iter().filter(|r| r["status"] == 200).count();
    let unreachable_count = results.iter().filter(|r| r["status"].is_null()).count();
    assert_eq!(ok_count, 1, "the live instance must succeed: {body}");
    assert_eq!(
        unreachable_count, 1,
        "the dead instance must be reported unreachable, not silently dropped: {body}"
    );

    drop(instances);
    drop(aggregator);
    Ok(())
}
