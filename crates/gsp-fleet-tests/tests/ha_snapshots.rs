//! Catch-up by snapshot: once a tier's Raft log has been purged, a node that
//! joins afterwards cannot be sent the log from the start and must install a
//! snapshot, which has to carry every config revision at its number.
//!
//! The replicas run with the hidden `--ha-snapshot-after 10`, so a snapshot
//! is built every 10 entries. `openraft` still keeps the last 1000 entries
//! behind each snapshot (`max_in_snapshot_log_to_keep`, left at its
//! default), so the test writes more than that before the fourth node joins.

use std::time::Duration;

use anyhow::{ensure, Result};
use gsp_fleet_tests::{
    build_fleet_bins, free_port, minimal_gsp_config, spawn_controller_with, wait_until, Proc,
};

/// Comfortably past `max_in_snapshot_log_to_keep` (1000) plus one snapshot
/// interval, so the entries the joiner would need first are purged.
const WRITES: u16 = 1_100;
const HA_TOKEN: &str = "ha-snapshots-test-token";

/// `POST /config`; `Some(revision)` on 2xx.
async fn submit(client: &reqwest::Client, base: &str, body: String) -> Option<u64> {
    let resp = client
        .post(format!("{base}/config"))
        .body(body)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let v: serde_json::Value = resp.json().await.ok()?;
    v["revision"].as_u64()
}

/// `GET /config/revisions` as parsed JSON.
async fn revisions(client: &reqwest::Client, base: &str) -> Result<serde_json::Value> {
    Ok(client
        .get(format!("{base}/config/revisions"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

fn spawn_node(dir: &tempfile::TempDir, port: u16, extra: &[&str]) -> Result<Proc> {
    let mut args: Vec<String> = extra.iter().map(|s| s.to_string()).collect();
    args.extend(
        ["--ha-token", HA_TOKEN, "--ha-snapshot-after", "10"]
            .iter()
            .map(|s| s.to_string()),
    );
    spawn_controller_with(dir.path(), &format!("127.0.0.1:{port}"), &args)
}

#[tokio::test]
#[ignore = "needs --ha-join (Task 9)"]
async fn a_node_that_joins_after_a_purge_catches_up_by_snapshot() -> Result<()> {
    build_fleet_bins()?;
    let client = reqwest::Client::new();
    let ports = (0..3).map(|_| free_port()).collect::<Result<Vec<_>>>()?;
    let peers = ports
        .iter()
        .enumerate()
        .map(|(i, p)| format!("{}=127.0.0.1:{p}", i + 1))
        .collect::<Vec<_>>()
        .join(",");
    let mut procs = Vec::new();
    let mut dirs = Vec::new();
    for (i, p) in ports.iter().enumerate() {
        let dir = tempfile::tempdir()?;
        let id = (i + 1).to_string();
        procs.push(spawn_node(
            &dir,
            *p,
            &["--ha-node-id", &id, "--ha-peers", &peers],
        )?);
        dirs.push(dir);
    }
    let bases: Vec<String> = ports
        .iter()
        .map(|p| format!("http://127.0.0.1:{p}"))
        .collect();

    // Distinct bodies (the target port varies) so every write is a revision.
    let (admin, listen) = (free_port()?, free_port()?);
    let body = |i: u16| minimal_gsp_config(admin, listen, 20_000 + i);
    wait_until(
        || {
            let (client, base, body) = (client.clone(), bases[0].clone(), body(0));
            async move { Ok(submit(&client, &base, body).await.is_some()) }
        },
        Duration::from_secs(60),
        "a raft leader to accept a write",
    )
    .await?;
    for i in 1..WRITES {
        ensure!(
            submit(&client, &bases[0], body(i)).await.is_some(),
            "write {i} was refused"
        );
    }

    // A fourth node joins the running cluster; the leader can only catch it
    // up by snapshot, since the start of its log is gone.
    let port4 = free_port()?;
    let dir4 = tempfile::tempdir()?;
    procs.push(spawn_node(
        &dir4,
        port4,
        &["--ha-join", "--ha-node-id", "4"],
    )?);
    dirs.push(dir4);
    let base4 = format!("http://127.0.0.1:{port4}");
    let added = client
        .post(format!("{}/admin/ha/members", bases[0]))
        .json(&serde_json::json!({ "id": 4, "addr": format!("127.0.0.1:{port4}") }))
        .send()
        .await?;
    ensure!(
        added.status().is_success(),
        "adding node 4 failed: {}",
        added.text().await?
    );

    // Same revisions, at the same numbers, with the same metadata.
    let want = revisions(&client, &bases[0]).await?;
    ensure!(
        want.as_array().map(Vec::len) == Some(usize::from(WRITES)),
        "the leader holds {} revisions, expected {WRITES}",
        want.as_array().map_or(0, Vec::len)
    );
    wait_until(
        || {
            let (client, base4, want) = (client.clone(), base4.clone(), want.clone());
            async move { Ok(revisions(&client, &base4).await.ok() == Some(want)) }
        },
        Duration::from_secs(30),
        "node 4 to hold the leader's revisions",
    )
    .await?;

    for p in &mut procs {
        ensure!(p.exit_code().is_none(), "a node exited:\n{}", p.log());
    }
    Ok(())
}
