//! Intra-tier HA with every replica reachable by its peers only through a
//! TLS terminator signed by a private CA (`--ha-peers id=https://…` +
//! `--ca-file`): Raft RPCs and write forwarding both cross TLS.

use std::time::Duration;

use anyhow::{ensure, Result};
use gsp_fleet_tests::tls_front::{tls_front, TEST_CA};
use gsp_fleet_tests::{
    build_fleet_bins, free_port, minimal_gsp_config, spawn_controller_with, wait_until, Proc,
};
use tokio::task::JoinHandle;

struct Cluster {
    plain: Vec<String>,
    _procs: Vec<Proc>,
    _fronts: Vec<JoinHandle<()>>,
    _dirs: Vec<tempfile::TempDir>,
}

/// Three replicas on plain loopback ports, each fronted by `tls_front`;
/// peers know each other only by the fronts' `https://localhost` URLs.
async fn cluster(with_ca: bool) -> Result<Cluster> {
    build_fleet_bins()?;
    let mut plain_ports = Vec::new();
    let mut fronts = Vec::new();
    let mut front_ports = Vec::new();
    for _ in 0..3 {
        let p = free_port()?;
        let (front, task) = tls_front(([127, 0, 0, 1], p).into()).await?;
        plain_ports.push(p);
        front_ports.push(front.port());
        fronts.push(task);
    }
    let peers = front_ports
        .iter()
        .enumerate()
        .map(|(i, f)| format!("{}=https://localhost:{f}", i + 1))
        .collect::<Vec<_>>()
        .join(",");
    let mut procs = Vec::new();
    let mut dirs = Vec::new();
    for (i, p) in plain_ports.iter().enumerate() {
        let dir = tempfile::tempdir()?;
        let mut extra = vec![
            "--ha-node-id".to_string(),
            (i + 1).to_string(),
            "--ha-peers".to_string(),
            peers.clone(),
        ];
        if with_ca {
            extra.extend(["--ca-file".to_string(), TEST_CA.to_string()]);
        }
        procs.push(spawn_controller_with(
            dir.path(),
            &format!("127.0.0.1:{p}"),
            &extra,
        )?);
        dirs.push(dir);
    }
    Ok(Cluster {
        plain: plain_ports
            .iter()
            .map(|p| format!("http://127.0.0.1:{p}"))
            .collect(),
        _procs: procs,
        _fronts: fronts,
        _dirs: dirs,
    })
}

/// `POST /config`; `Some(revision)` on 2xx.
async fn submit(base: &str, body: String) -> Option<u64> {
    let resp = reqwest::Client::new()
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

fn config() -> Result<String> {
    Ok(minimal_gsp_config(free_port()?, free_port()?, free_port()?))
}

#[tokio::test]
async fn three_replicas_replicate_through_tls_terminators() -> Result<()> {
    let c = cluster(true).await?;

    let first = config()?;
    wait_until(
        || {
            let (base, body) = (c.plain[0].clone(), first.clone());
            async move { Ok(submit(&base, body).await.is_some()) }
        },
        Duration::from_secs(60),
        "a raft leader to accept a write",
    )
    .await?;

    // One write per replica: at least two land on followers and are
    // forwarded to the leader's https URL.
    let mut last = (0, String::new());
    for base in &c.plain {
        let body = config()?;
        let rev = submit(base, body.clone()).await;
        ensure!(rev.is_some(), "write via {base} was not accepted");
        last = (rev.unwrap(), body);
    }

    let (want_rev, want_body) = last;
    wait_until(
        || {
            let (plain, want_body) = (c.plain.clone(), want_body.clone());
            async move {
                for base in &plain {
                    let resp = reqwest::get(format!("{base}/config")).await?;
                    let rev = resp
                        .headers()
                        .get("x-config-revision")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<u64>().ok());
                    if rev != Some(want_rev) || resp.text().await? != want_body {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
        },
        Duration::from_secs(30),
        "every replica to serve the last write",
    )
    .await?;
    Ok(())
}

/// Guards the positive test: without the CA no Raft RPC verifies, so no
/// leader is ever elected and every write is refused.
#[tokio::test]
async fn replicas_without_the_ca_never_elect_a_leader() -> Result<()> {
    let c = cluster(false).await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    while tokio::time::Instant::now() < deadline {
        ensure!(
            submit(&c.plain[0], config()?).await.is_none(),
            "a write was accepted although no replica trusts its peers' certificates"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Ok(())
}
