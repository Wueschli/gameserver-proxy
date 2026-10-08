//! `--tunnel-network` together with `--ha-peers`: the registries and the
//! address book replicate through Raft, so a registration made on one node
//! is seen — with the same tunnel address — on the others
//! (`docs/superpowers/specs/2026-10-03-ha-replicated-address-allocation-design.md`).
//! Plain `cargo test`: no namespaces needed.

use std::time::Duration;

use anyhow::{anyhow, ensure, Result};
use serde_json::{json, Value};
use wayhouse_fleet_tests::{
    build_fleet_bins, free_port, spawn_controller_with, wait_http_up, wait_until, Proc,
};

const KEY: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

struct Cluster {
    bases: Vec<String>,
    /// `None` once a node has been killed.
    procs: Vec<Option<Proc>>,
    http: reqwest::Client,
    _dirs: Vec<tempfile::TempDir>,
}

async fn cluster() -> Result<Cluster> {
    Cluster::start(None, false).await
}

impl Cluster {
    /// Three nodes on loopback with `--tunnel-network 10.60.0.0/24`. With
    /// `seed`, node 1 starts on that (pre-HA) data directory; with
    /// `node1_last` it starts only after nodes 2 and 3 have elected a leader.
    async fn start(seed: Option<tempfile::TempDir>, node1_last: bool) -> Result<Cluster> {
        build_fleet_bins()?;
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
        let dirs = (0..3)
            .map(|_| tempfile::tempdir())
            .collect::<std::io::Result<Vec<_>>>()?;
        let mut c = Cluster {
            bases,
            procs: vec![None, None, None],
            http: wayhouse_http::client(),
            _dirs: Vec::new(),
        };
        let spawn = |i: usize, dir: &std::path::Path| -> Result<Proc> {
            let extra = [
                "--ha-node-id",
                &(i + 1).to_string(),
                "--ha-peers",
                &peers,
                "--tunnel-network",
                "10.60.0.0/24",
            ]
            .map(String::from);
            spawn_controller_with(dir, &format!("127.0.0.1:{}", ports[i]), &extra)
        };
        // Node 1's directory is the seed's when there is one.
        let node1_dir = match &seed {
            Some(seed) => seed.path().to_path_buf(),
            None => dirs[0].path().to_path_buf(),
        };
        let dir_of = |i: usize| {
            if i == 0 {
                node1_dir.clone()
            } else {
                dirs[i].path().to_path_buf()
            }
        };
        let order: Vec<usize> = if node1_last {
            vec![1, 2, 0]
        } else {
            vec![0, 1, 2]
        };
        for (n, &i) in order.iter().enumerate() {
            if node1_last && n == 2 {
                // Nodes 2 and 3 are a majority: wait for them to elect a leader.
                wait_until(
                    || {
                        let c = &c;
                        async move { Ok(c.leader().await.is_ok()) }
                    },
                    Duration::from_secs(15),
                    "nodes 2 and 3 to elect a leader without node 1",
                )
                .await?;
            }
            c.procs[i] = Some(spawn(i, &dir_of(i))?);
            wait_http_up(&format!("{}/healthz", c.bases[i]), Duration::from_secs(10)).await?;
        }
        c._dirs = dirs;
        if let Some(seed) = seed {
            c._dirs.push(seed);
        }
        Ok(c)
    }

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

/// A pre-HA controller on `--tunnel-network 10.60.0.0/24` registers three
/// origins and stops; returns its data directory and the addresses it gave.
async fn pre_ha_controller() -> Result<(tempfile::TempDir, Vec<(String, Value)>)> {
    build_fleet_bins()?;
    let dir = tempfile::tempdir()?;
    let port = free_port()?;
    let base = format!("http://127.0.0.1:{port}");
    let ctl = spawn_controller_with(
        dir.path(),
        &format!("127.0.0.1:{port}"),
        &["--tunnel-network".to_string(), "10.60.0.0/24".to_string()],
    )?;
    wait_http_up(&format!("{base}/healthz"), Duration::from_secs(10)).await?;
    let http = wayhouse_http::client();
    let mut given = Vec::new();
    for name in ["o1", "o2", "o3"] {
        let r: Value = http
            .post(format!("{base}/peers"))
            .json(&json!({"name": name, "pubkey": KEY, "backends": [":25565"]}))
            .send()
            .await?
            .json()
            .await?;
        given.push((name.to_string(), r["tunnel_address"].clone()));
    }
    ctl.kill().await?;
    Ok((dir, given))
}

/// Waits until `GET /peers` on `base` lists exactly `given` with the same
/// addresses.
async fn assert_imported(c: &Cluster, base: &str, given: &[(String, Value)]) -> Result<()> {
    let (http, url) = (c.http.clone(), format!("{base}/peers"));
    wait_until(
        || {
            let (http, url) = (http.clone(), url.clone());
            async move {
                let Ok(r) = http.get(url).send().await else {
                    return Ok(false);
                };
                if !r.status().is_success() {
                    return Ok(false);
                }
                let listed: Value = r.json().await?;
                let Some(list) = listed.as_array() else {
                    return Ok(false);
                };
                Ok(list.len() == 3)
            }
        },
        Duration::from_secs(20),
        "the imported origins to be listed",
    )
    .await?;
    for (name, address) in given {
        let r: Value = c
            .http
            .get(format!("{base}/peers/{name}"))
            .send()
            .await?
            .json()
            .await?;
        ensure!(
            &r["tunnel_address"] == address,
            "{name} moved from {address} to {}",
            r["tunnel_address"]
        );
    }
    Ok(())
}

#[tokio::test]
async fn a_single_node_upgrades_without_losing_addresses() -> Result<()> {
    let (dir, given) = pre_ha_controller().await?;
    let c = Cluster::start(Some(dir), false).await?;
    assert_imported(&c, &c.bases[1], &given).await?;
    // The set-aside data is kept, and a new origin gets the next address.
    let fresh = c.register(&c.bases[2], "o4").await?;
    ensure!(fresh["tunnel_address"] == "10.60.0.4", "{fresh}");
    Ok(())
}

#[tokio::test]
async fn import_happens_when_an_empty_node_leads() -> Result<()> {
    let (dir, given) = pre_ha_controller().await?;
    let c = Cluster::start(Some(dir), true).await?;
    assert_imported(&c, &c.bases[1], &given).await
}

/// Starts node `id` (a fresh directory) with `--ha-join` on a free port.
fn spawn_joiner(dir: &std::path::Path, id: u64, port: u16) -> Result<Proc> {
    let extra = [
        "--ha-join",
        "--ha-node-id",
        &id.to_string(),
        "--tunnel-network",
        "10.60.0.0/24",
    ]
    .map(String::from);
    spawn_controller_with(dir, &format!("127.0.0.1:{port}"), &extra)
}

impl Cluster {
    /// `POST`/`PUT`/`DELETE` a membership change on `base`.
    async fn member_change(
        &self,
        method: reqwest::Method,
        base: &str,
        path: &str,
        body: Value,
    ) -> Result<()> {
        let r = self
            .http
            .request(method, format!("{base}{path}"))
            .json(&body)
            .send()
            .await?;
        let status = r.status();
        if !status.is_success() {
            // A 500 here is a controller bug (#257): the nodes' own output
            // is the only place its cause shows.
            let logs: String = self
                .procs
                .iter()
                .flatten()
                .map(|p| format!("--- {} ---\n{}\n", p.name(), p.log()))
                .collect();
            return Err(anyhow!("{path}: {status} {}\n{logs}", r.text().await?));
        }
        Ok(())
    }

    /// Waits until `GET {base}/peers/{name}` answers 200.
    async fn served(&self, base: &str, name: &str, what: &str) -> Result<()> {
        let (http, url) = (self.http.clone(), format!("{base}/peers/{name}"));
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
            Duration::from_secs(20),
            what,
        )
        .await
    }
}

#[tokio::test]
async fn a_fourth_node_joins_then_the_leader_is_removed() -> Result<()> {
    let mut c = cluster().await?;
    c.register(&c.bases[0], "o1").await?;

    let port4 = free_port()?;
    let dir4 = tempfile::tempdir()?;
    let node4 = spawn_joiner(dir4.path(), 4, port4)?;
    let base4 = format!("http://127.0.0.1:{port4}");
    wait_http_up(&format!("{base4}/healthz"), Duration::from_secs(10)).await?;
    // Asked of a node that may be a follower: it forwards to the leader.
    c.member_change(
        reqwest::Method::POST,
        &c.bases[1],
        "/admin/ha/members",
        json!({"id": 4, "addr": format!("127.0.0.1:{port4}")}),
    )
    .await?;
    c.served(&base4, "o1", "node 4 to serve the cluster's origin")
        .await?;
    let members: Value = c
        .http
        .get(format!("{}/admin/ha/members", c.bases[0]))
        .send()
        .await?
        .json()
        .await?;
    ensure!(
        members["voters"].as_array().map(Vec::len) == Some(4),
        "{members}"
    );

    // Remove the leader, asked of another node.
    let leader = c.leader().await?;
    let slot = (leader - 1) as usize;
    let other = c.bases[(slot + 1) % 3].clone();
    c.member_change(
        reqwest::Method::DELETE,
        &other,
        &format!("/admin/ha/members/{leader}"),
        json!({}),
    )
    .await?;
    c.procs[slot].take().unwrap().kill().await?;
    let second = c.register(&other, "o2").await?;
    ensure!(second["tunnel_address"].is_string(), "{second}");
    drop(node4);
    Ok(())
}

#[tokio::test]
async fn put_moves_a_member_to_a_new_port() -> Result<()> {
    let mut c = cluster().await?;
    c.register(&c.bases[0], "o1").await?;
    // Never the leader's slot: node 3 is simply the one that moves.
    c.procs[2].take().unwrap().kill().await?;
    let new_port = free_port()?;
    let _moved = spawn_joiner(c._dirs[2].path(), 3, new_port)?;
    let new_base = format!("http://127.0.0.1:{new_port}");
    wait_http_up(&format!("{new_base}/healthz"), Duration::from_secs(10)).await?;
    c.member_change(
        reqwest::Method::PUT,
        &c.bases[0],
        "/admin/ha/members/3",
        json!({"addr": format!("127.0.0.1:{new_port}")}),
    )
    .await?;
    c.register(&c.bases[0], "o2").await?;
    c.served(
        &new_base,
        "o2",
        "node 3 to receive registrations at its new port",
    )
    .await
}
