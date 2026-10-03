//! `--tunnel-network` together with `--ha-peers`: the registries and the
//! address book replicate through Raft, so a registration made on one node
//! is seen — with the same tunnel address — on the others
//! (`docs/superpowers/specs/2026-10-03-ha-replicated-address-allocation-design.md`).
//! Plain `cargo test`: no namespaces needed.

use std::time::Duration;

use anyhow::{anyhow, ensure, Result};
use gsp_fleet_tests::{
    build_fleet_bins, free_port, spawn_controller_with, wait_http_up, wait_until, Proc,
};
use serde_json::{json, Value};

const KEY: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

struct Cluster {
    bases: Vec<String>,
    /// `None` once a node has been killed.
    procs: Vec<Option<Proc>>,
    http: reqwest::Client,
    _dirs: Vec<tempfile::TempDir>,
}

async fn cluster() -> Result<Cluster> {
    build_fleet_bins()?;
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
        let extra = [
            "--ha-node-id",
            &(i + 1).to_string(),
            "--ha-peers",
            &peers,
            "--tunnel-network",
            "10.60.0.0/24",
        ]
        .map(String::from);
        procs.push(Some(spawn_controller_with(
            dir.path(),
            &format!("127.0.0.1:{p}"),
            &extra,
        )?));
        dirs.push(dir);
    }
    let bases: Vec<String> = ports
        .iter()
        .map(|p| format!("http://127.0.0.1:{p}"))
        .collect();
    for b in &bases {
        wait_http_up(&format!("{b}/healthz"), Duration::from_secs(10)).await?;
    }
    Ok(Cluster {
        bases,
        procs,
        http: reqwest::Client::new(),
        _dirs: dirs,
    })
}

impl Cluster {
    /// The leader's node id, as any live node reports it.
    async fn leader(&self) -> Result<u64> {
        for (i, b) in self.bases.iter().enumerate() {
            if self.procs[i].is_none() {
                continue;
            }
            let Ok(r) = self.http.get(format!("{b}/admin/ha/members")).send().await else {
                continue;
            };
            if let Some(id) = r.json::<Value>().await?["leader"].as_u64() {
                return Ok(id);
            }
        }
        Err(anyhow!("no leader reported"))
    }

    /// `POST /peers` on `base`, retried while the cluster is still
    /// initializing; returns the response body.
    async fn register(&self, base: &str, name: &str) -> Result<Value> {
        let mut last = String::new();
        for _ in 0..60 {
            let r = self
                .http
                .post(format!("{base}/peers"))
                .json(&json!({"name": name, "pubkey": KEY, "backends": [":25565"]}))
                .send()
                .await?;
            if r.status().is_success() {
                return Ok(r.json().await?);
            }
            last = format!("{} {}", r.status(), r.text().await?);
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        Err(anyhow!(
            "registering {name} on {base} never succeeded: {last}"
        ))
    }
}

#[tokio::test]
async fn an_origin_registered_on_one_node_is_seen_on_another() -> Result<()> {
    let c = cluster().await?;
    let made = c.register(&c.bases[0], "o1").await?;
    let address = made["tunnel_address"].clone();
    ensure!(address.is_string(), "no address allocated: {made}");

    let seen = c.bases[2].clone();
    let http = c.http.clone();
    wait_until(
        || {
            let (http, url) = (http.clone(), format!("{seen}/peers/o1"));
            let address = address.clone();
            async move {
                let Ok(r) = http.get(url).send().await else {
                    return Ok(false);
                };
                if !r.status().is_success() {
                    return Ok(false);
                }
                Ok(r.json::<Value>().await?["tunnel_address"] == address)
            }
        },
        Duration::from_secs(5),
        "node 3 to serve the origin with the same tunnel address",
    )
    .await
}

#[tokio::test]
async fn losing_the_leader_keeps_registrations_working() -> Result<()> {
    let mut c = cluster().await?;
    let first = c.register(&c.bases[0], "o1").await?;
    let leader = c.leader().await?;
    let slot = (leader - 1) as usize;
    c.procs[slot].take().unwrap().kill().await?;

    let survivor = c
        .bases
        .iter()
        .enumerate()
        .find(|(i, _)| *i != slot)
        .map(|(_, b)| b.clone())
        .unwrap();
    // The survivors elect a new leader; registering retries until they have.
    let second = c.register(&survivor, "o2").await?;
    ensure!(
        first["tunnel_address"] != second["tunnel_address"],
        "two origins share {}",
        first["tunnel_address"]
    );
    Ok(())
}

/// Reads SSE `data:` payloads from `resp` until `n` have arrived.
async fn events(mut resp: reqwest::Response, n: usize) -> Result<Vec<Value>> {
    let mut buf = String::new();
    let mut out = Vec::new();
    while out.len() < n {
        let chunk = tokio::time::timeout(Duration::from_secs(5), resp.chunk())
            .await
            .map_err(|_| anyhow!("timed out after {} of {n} events", out.len()))??
            .ok_or_else(|| anyhow!("stream ended after {} of {n} events", out.len()))?;
        buf.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(end) = buf.find("\n\n") {
            let frame: String = buf.drain(..end + 2).collect();
            for line in frame.lines() {
                if let Some(data) = line.strip_prefix("data:") {
                    out.push(serde_json::from_str(data.trim())?);
                }
            }
        }
    }
    Ok(out)
}

#[tokio::test]
async fn a_subscriber_resumes_on_another_node_with_its_cursor() -> Result<()> {
    let c = cluster().await?;
    c.register(&c.bases[0], "o1").await?;
    c.register(&c.bases[0], "o2").await?;

    let first = c
        .http
        .get(format!("{}/peers/subscribe", c.bases[0]))
        .send()
        .await?;
    let seen = events(first, 2).await?;
    let cursor = seen[1]["revision"]
        .as_u64()
        .ok_or_else(|| anyhow!("no revision in {}", seen[1]))?;

    // Wait until node 2 holds the same revisions before resuming there.
    let (http, url) = (c.http.clone(), format!("{}/peers/o2", c.bases[1]));
    wait_until(
        || {
            let (http, url) = (http.clone(), url.clone());
            async move {
                Ok(http
                    .get(url)
                    .send()
                    .await
                    .is_ok_and(|r| r.status().is_success()))
            }
        },
        Duration::from_secs(5),
        "node 2 to hold o2",
    )
    .await?;

    let resumed = c
        .http
        .get(format!("{}/peers/subscribe?since={cursor}", c.bases[1]))
        .send()
        .await?;
    c.register(&c.bases[0], "o3").await?;
    let next = events(resumed, 1).await?;
    ensure!(
        next[0]["revision"].as_u64() == Some(cursor + 1),
        "expected exactly revision {}, got {}",
        cursor + 1,
        next[0]
    );
    ensure!(next[0].to_string().contains("o3"), "{}", next[0]);
    Ok(())
}
