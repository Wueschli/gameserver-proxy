//! `--auth-token` together with `--ha-peers`: a write that reaches a follower
//! is forwarded to the leader with the caller's own `Authorization`, so it
//! succeeds whichever replica the caller picked; a caller without the token
//! is still refused. Plain `cargo test`: no namespaces needed.

use std::time::Duration;

use anyhow::{ensure, Result};
use serde_json::{json, Value};
use wayhouse_fleet_tests::{
    build_fleet_bins, free_port, minimal_wayhouse_config, spawn_controller_with, wait_http_up, Proc,
};

const AUTH_TOKEN: &str = "ha-auth-test-client-token";
const KEY: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

#[tokio::test]
async fn writes_through_a_follower_carry_the_callers_bearer_token() -> Result<()> {
    build_fleet_bins()?;
    let http = wayhouse_http::client();
    let ports = (0..3).map(|_| free_port()).collect::<Result<Vec<_>>>()?;
    let peers = ports
        .iter()
        .enumerate()
        .map(|(i, p)| format!("{}=127.0.0.1:{p}", i + 1))
        .collect::<Vec<_>>()
        .join(",");
    let bases: Vec<String> = ports
        .iter()
        .map(|p| format!("http://127.0.0.1:{p}"))
        .collect();
    let mut procs: Vec<Proc> = Vec::new();
    let mut dirs = Vec::new();
    for (i, p) in ports.iter().enumerate() {
        let dir = tempfile::tempdir()?;
        let extra = [
            "--ha-node-id",
            &(i + 1).to_string(),
            "--ha-peers",
            &peers,
            "--auth-token",
            AUTH_TOKEN,
            "--tunnel-network",
            "10.60.0.0/24",
        ]
        .map(String::from);
        procs.push(spawn_controller_with(
            dir.path(),
            &format!("127.0.0.1:{p}"),
            &extra,
        )?);
        dirs.push(dir);
        wait_http_up(&format!("{}/healthz", bases[i]), Duration::from_secs(10)).await?;
    }

    // Writing through every node covers both roles: whichever replica is a
    // follower forwards to the leader.
    let config = minimal_wayhouse_config(free_port()?, free_port()?, free_port()?);
    let mut last = String::new();
    let mut accepted = 0;
    for round in 0..3 {
        for base in &bases {
            // Retried while the cluster is still electing a leader.
            let mut ok = false;
            for _ in 0..60 {
                let r = http
                    .post(format!("{base}/config"))
                    .bearer_auth(AUTH_TOKEN)
                    .body(config.clone())
                    .send()
                    .await?;
                if r.status().is_success() {
                    ok = true;
                    break;
                }
                last = format!("{} {}", r.status(), r.text().await?);
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            ensure!(
                ok,
                "round {round}: POST /config on {base} never succeeded: {last}"
            );
            accepted += 1;
        }
    }
    ensure!(accepted == 9);

    // Registrations forward too, and carry a JSON body through. Retried
    // while the cluster is still initializing its registries.
    for (i, base) in bases.iter().enumerate() {
        let body = json!({"name": format!("o{i}"), "pubkey": KEY, "backends": [":25565"]});
        let mut last = String::new();
        let mut ok = false;
        for _ in 0..60 {
            let r = http
                .post(format!("{base}/peers"))
                .bearer_auth(AUTH_TOKEN)
                .json(&body)
                .send()
                .await?;
            if r.status().is_success() {
                ok = true;
                break;
            }
            last = format!("{} {}", r.status(), r.text().await?);
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        ensure!(ok, "POST /peers on {base} never succeeded: {last}");
    }

    // A caller without the token, or with the wrong one, is refused on every node.
    for base in &bases {
        let none = http
            .post(format!("{base}/config"))
            .body(config.clone())
            .send()
            .await?;
        ensure!(
            none.status() == 401,
            "{base}: no token gave {}",
            none.status()
        );
        let wrong = http
            .post(format!("{base}/config"))
            .bearer_auth("not-the-token-at-all")
            .body(config.clone())
            .send()
            .await?;
        ensure!(
            wrong.status() == 401,
            "{base}: wrong token gave {}",
            wrong.status()
        );
    }

    let revs: Value = http
        .get(format!("{}/config/revisions", bases[0]))
        .bearer_auth(AUTH_TOKEN)
        .send()
        .await?
        .json()
        .await?;
    ensure!(
        revs.as_array().is_some_and(|a| !a.is_empty()),
        "no revisions listed: {revs}"
    );
    drop(procs);
    Ok(())
}
