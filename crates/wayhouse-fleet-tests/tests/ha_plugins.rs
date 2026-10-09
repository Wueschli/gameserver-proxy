//! `--plugins` together with `--ha-peers`: a module uploaded and installed through a
//! follower is forwarded to the leader and replicated (install and module) to every replica, only the leader
//! runs the ticks (a follower's status read is forwarded to it), and when the leader dies
//! the new leader takes over the schedule. A second test sets a secret and enables a webhook
//! through a follower, calls the webhook through a follower and checks that no node's files
//! hold the secret. Plain `cargo test`: no namespaces needed.

use std::time::Duration;

use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use wayhouse_fleet_tests::{
    build_fleet_bins, free_port, spawn_controller_with, wait_http_up, wait_until, Proc,
};

const AUTH_TOKEN: &str = "ha-plugins-test-client-token";
const CAPS: &str =
    r#"{"triggers":{"on_timer":true},"tick_interval_secs":10,"log":true,"state":{"max_bytes":64}}"#;

/// Counts its ticks in the byte under key `n`, so every tick writes state (a Raft entry).
fn counter_module() -> Vec<u8> {
    let caps = CAPS.replace('"', "\\\"");
    wat::parse_str(format!(
        r#"(module
  (@custom "wayhouse.plugin-abi" "\00\00\01\00")
  (@custom "wayhouse.plugin-caps" "{caps}")
  (import "wayhouse" "log" (func $log (param i32 i32 i32)))
  (import "wayhouse" "state_get" (func $get (param i32 i32 i32 i32) (result i32)))
  (import "wayhouse" "state_put" (func $put (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "tick")
  (data (i32.const 16) "n")
  (global $bump (mut i32) (i32.const 1024))
  (func (export "alloc") (param i32) (result i32)
    (local $p i32)
    (local.set $p (global.get $bump))
    (global.set $bump (i32.add (global.get $bump) (local.get 0)))
    (local.get $p))
  (func (export "init") (param i32 i32))
  (func (export "on_timer")
    (call $log (i32.const 2) (i32.const 0) (i32.const 4))
    (drop (call $get (i32.const 16) (i32.const 1) (i32.const 100) (i32.const 1)))
    (i32.store8 (i32.const 100) (i32.add (i32.load8_u (i32.const 100)) (i32.const 1)))
    (drop (call $put (i32.const 16) (i32.const 1) (i32.const 100) (i32.const 1)))))"#
    ))
    .expect("the counter module assembles")
}

async fn leader_id(http: &reqwest::Client, base: &str) -> Option<u64> {
    let r = http
        .get(format!("{base}/admin/ha/members"))
        .bearer_auth(AUTH_TOKEN)
        .send()
        .await
        .ok()?;
    r.json::<Value>().await.ok()?["leader"].as_u64()
}

#[tokio::test]
async fn plugins_replicate_through_a_follower_and_the_new_leader_keeps_ticking() -> Result<()> {
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
    let mut procs: Vec<Option<Proc>> = Vec::new();
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
            "10.61.0.0/24",
            "--plugins",
        ]
        .map(String::from);
        procs.push(Some(spawn_controller_with(
            dir.path(),
            &format!("127.0.0.1:{p}"),
            &extra,
        )?));
        dirs.push(dir);
        wait_http_up(&format!("{}/healthz", bases[i]), Duration::from_secs(10)).await?;
    }

    // Wait for a leader every node agrees on, then pick a follower.
    wait_until(
        || async {
            let mut seen = Vec::new();
            for b in &bases {
                seen.push(leader_id(&http, b).await);
            }
            Ok(seen[0].is_some() && seen.iter().all(|s| *s == seen[0]))
        },
        Duration::from_secs(20),
        "all nodes agree on a leader",
    )
    .await?;
    let leader = leader_id(&http, &bases[0]).await.context("a leader")? as usize - 1;
    let follower = (leader + 1) % 3;

    // Upload and install through the follower: both are forwarded to the leader.
    let module = counter_module();
    let up = http
        .post(format!("{}/plugins/modules", bases[follower]))
        .bearer_auth(AUTH_TOKEN)
        .body(module.clone())
        .send()
        .await?;
    ensure!(up.status() == 201, "upload via follower: {}", up.status());
    let sha = up.json::<Value>().await?["sha256"]
        .as_str()
        .context("sha256")?
        .to_string();
    let caps: Value = serde_json::from_str(CAPS)?;
    let installed = http
        .post(format!("{}/plugins", bases[follower]))
        .bearer_auth(AUTH_TOKEN)
        .header("x-actor", "fleet-test")
        .json(&json!({"name": "counter", "sha256": sha, "approved": caps}))
        .send()
        .await?;
    ensure!(
        installed.status() == 201,
        "install via follower: {}",
        installed.status()
    );
    let record: Value = installed.json().await?;
    let id = record["id"].as_str().context("id")?.to_string();
    ensure!(record["created_by"] == "fleet-test", "{record}");

    // Every replica ends up with the install (reads are local).
    for base in &bases {
        wait_until(
            || async {
                let r = http
                    .get(format!("{base}/plugins"))
                    .bearer_auth(AUTH_TOKEN)
                    .send()
                    .await?;
                let v: Value = r.json().await?;
                Ok(v.as_array()
                    .is_some_and(|a| a.len() == 1 && a[0]["id"] == id.as_str()))
            },
            Duration::from_secs(15),
            "the install is on every replica",
        )
        .await?;
    }

    // The leader ticks (after a 10 s grace) and commits through Raft; a follower's status
    // read is forwarded to it.
    let status_url = |i: usize| format!("{}/plugins/{id}/status", bases[i]);
    wait_until(
        || async {
            let v: Value = http
                .get(status_url(follower))
                .bearer_auth(AUTH_TOKEN)
                .send()
                .await?
                .json()
                .await?;
            Ok(v["ticks"].as_u64().unwrap_or(0) >= 1 && v["last_ok"] == true)
        },
        Duration::from_secs(40),
        "the leader ran a tick and committed its state",
    )
    .await?;

    // Kill the leader: a new one is elected and starts ticking the replicated install.
    // The module was pushed to a quorum before the install was recorded and the other
    // replica fetches it, so whichever node wins runs it: the tick succeeds.
    procs[leader].take().context("leader proc")?.kill().await?;
    let survivor = follower;
    let other = 3 - leader - follower;
    wait_until(
        || async {
            let l = leader_id(&http, &bases[survivor]).await;
            Ok(l.is_some_and(|l| l as usize - 1 != leader))
        },
        Duration::from_secs(30),
        "a new leader is elected",
    )
    .await?;
    wait_until(
        || async {
            let r = http
                .get(status_url(other))
                .bearer_auth(AUTH_TOKEN)
                .send()
                .await?;
            if !r.status().is_success() {
                return Ok(false);
            }
            let v: Value = r.json().await?;
            Ok(v["ticks"].as_u64().unwrap_or(0) >= 1 && v["last_ok"] == true)
        },
        Duration::from_secs(60),
        "the new leader runs the replicated module",
    )
    .await?;

    // Deleting through a survivor removes it everywhere left.
    let del = http
        .delete(format!("{}/plugins/{id}", bases[other]))
        .bearer_auth(AUTH_TOKEN)
        .send()
        .await?;
    ensure!(del.status() == 204, "delete: {}", del.status());
    drop(procs);
    Ok(())
}

const HOOK_CAPS: &str = r#"{"triggers":{"on_timer":true,"on_webhook":true},"tick_interval_secs":30,"log":true,"http":{"hosts":[{"host":"panel.example"}]},"secrets":[{"name":"PANEL_TOKEN","hosts":["panel.example"]}]}"#;
const KEY: &str = "a2tra2tra2tra2tra2tra2tra2tra2tra2tra2tra2s=";
const SENTINEL: &str = "sentinel-secret-value-9f3a1c77";
const SENTINEL_B64: &str = "c2VudGluZWwtc2VjcmV0LXZhbHVlLTlmM2ExYzc3";

/// `on_webhook` answers `202` with the body `ok`.
fn hook_module() -> Vec<u8> {
    let caps = HOOK_CAPS.replace('"', "\\\"");
    let resp = r#"{"status":202,"headers":{},"body":"b2s="}"#;
    let (len, resp) = (resp.len(), resp.replace('"', "\\\""));
    wat::parse_str(format!(
        r#"(module
  (@custom "wayhouse.plugin-abi" "\00\00\01\00")
  (@custom "wayhouse.plugin-caps" "{caps}")
  (import "wayhouse" "log" (func $log (param i32 i32 i32)))
  (import "wayhouse" "webhook_respond" (func $respond (param i32 i32) (result i32)))
  (memory (export "memory") 2)
  (data (i32.const 60000) "{resp}")
  (global $bump (mut i32) (i32.const 1024))
  (func (export "alloc") (param i32) (result i32) (local $p i32)
    (local.set $p (global.get $bump))
    (global.set $bump (i32.add (global.get $bump) (local.get 0)))
    (local.get $p))
  (func (export "init") (param i32 i32))
  (func (export "on_timer"))
  (func (export "on_webhook") (param i32 i32)
    (drop (call $respond (i32.const 60000) (i32.const {len})))))"#
    ))
    .expect("the hook module assembles")
}

fn files_under(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            files_under(&p, out);
        } else {
            out.push(p);
        }
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

#[tokio::test]
async fn a_secret_and_a_webhook_go_through_a_follower_and_no_node_stores_the_secret() -> Result<()>
{
    use std::os::unix::fs::PermissionsExt;
    build_fleet_bins()?;
    let http = wayhouse_http::client();
    let keydir = tempfile::tempdir()?;
    let keyfile = keydir.path().join("keys");
    std::fs::write(&keyfile, format!("{KEY}\n"))?;
    std::fs::set_permissions(&keyfile, std::fs::Permissions::from_mode(0o600))?;
    let ports = (0..3).map(|_| free_port()).collect::<Result<Vec<_>>>()?;
    let hook_ports = (0..3).map(|_| free_port()).collect::<Result<Vec<_>>>()?;
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
    let hooks: Vec<String> = hook_ports
        .iter()
        .map(|p| format!("http://127.0.0.1:{p}"))
        .collect();
    let mut procs = Vec::new();
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
            "10.62.0.0/24",
            "--plugins",
            "--plugin-secret-key-file",
            &keyfile.display().to_string(),
            "--plugin-webhook-listen",
            &format!("127.0.0.1:{}", hook_ports[i]),
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
    wait_until(
        || async {
            let mut seen = Vec::new();
            for b in &bases {
                seen.push(leader_id(&http, b).await);
            }
            Ok(seen[0].is_some() && seen.iter().all(|s| *s == seen[0]))
        },
        Duration::from_secs(20),
        "all nodes agree on a leader",
    )
    .await?;
    let leader = leader_id(&http, &bases[0]).await.context("a leader")? as usize - 1;
    let follower = (leader + 1) % 3;

    // Install through the follower.
    let up = http
        .post(format!("{}/plugins/modules", bases[follower]))
        .bearer_auth(AUTH_TOKEN)
        .body(hook_module())
        .send()
        .await?;
    ensure!(up.status() == 201, "upload: {}", up.status());
    let sha = up.json::<Value>().await?["sha256"]
        .as_str()
        .context("sha256")?
        .to_string();
    let caps: Value = serde_json::from_str(HOOK_CAPS)?;
    let installed = http
        .post(format!("{}/plugins", bases[follower]))
        .bearer_auth(AUTH_TOKEN)
        .json(&json!({"name": "hooked-plugin", "sha256": sha, "approved": caps}))
        .send()
        .await?;
    ensure!(installed.status() == 201, "install: {}", installed.status());
    let id = installed.json::<Value>().await?["id"]
        .as_str()
        .context("id")?
        .to_string();

    // The secret is sealed on the follower that got the request and replicated as ciphertext.
    let put = http
        .put(format!(
            "{}/plugins/{id}/secrets/PANEL_TOKEN",
            bases[follower]
        ))
        .bearer_auth(AUTH_TOKEN)
        .json(&json!({ "value": SENTINEL }))
        .send()
        .await?;
    ensure!(put.status() == 200, "secret via follower: {}", put.status());

    // The webhook is enabled through the follower; the leader mints the token.
    let enabled = http
        .post(format!("{}/plugins/{id}/webhook", bases[follower]))
        .bearer_auth(AUTH_TOKEN)
        .send()
        .await?;
    ensure!(enabled.status() == 200, "enable: {}", enabled.status());
    let shown: Value = enabled.json().await?;
    let token = shown["token"].as_str().context("token")?.to_string();
    let path = shown["path"].as_str().context("path")?.to_string();

    // Every replica holds the secret and the webhook hash (reads are local).
    for base in &bases {
        wait_until(
            || async {
                let secrets: Value = http
                    .get(format!("{base}/plugins/{id}/secrets"))
                    .bearer_auth(AUTH_TOKEN)
                    .send()
                    .await?
                    .json()
                    .await?;
                let rec: Value = http
                    .get(format!("{base}/plugins/{id}"))
                    .bearer_auth(AUTH_TOKEN)
                    .send()
                    .await?
                    .json()
                    .await?;
                Ok(secrets.to_string().contains("\"set\":true") && !rec["webhook"].is_null())
            },
            Duration::from_secs(15),
            "the secret and the webhook reached every replica",
        )
        .await?;
    }

    // Called on the follower's webhook listener, the request is forwarded to the leader
    // (loopback peers count as private enough); called on the leader, it runs there.
    for i in [follower, leader] {
        let r = http
            .post(format!("{}{path}", hooks[i]))
            .bearer_auth(&token)
            .body("{}")
            .send()
            .await?;
        ensure!(r.status() == 202, "hook on node {i}: {}", r.status());
        ensure!(r.text().await? == "ok", "hook body on node {i}");
    }
    let bad = http
        .post(format!("{}{path}", hooks[follower]))
        .bearer_auth("whk_wrong")
        .send()
        .await?;
    ensure!(bad.status() == 401, "wrong token: {}", bad.status());
    // The admin port does not serve hooks.
    let admin = http
        .post(format!("{}{path}", bases[follower]))
        .bearer_auth(&token)
        .send()
        .await?;
    ensure!(
        admin.status() != 202,
        "hooks must not be served on the admin port"
    );

    // No node's files hold the secret or its base64. Sled flushes within a second; the install
    // name must show up in the scanned files, so the scan is not vacuous.
    tokio::time::sleep(Duration::from_secs(3)).await;
    for (n, dir) in dirs.iter().enumerate() {
        let mut files = Vec::new();
        files_under(dir.path(), &mut files);
        let mut saw_install = false;
        for f in files {
            let bytes = std::fs::read(&f).unwrap_or_default();
            for needle in [SENTINEL.as_bytes(), SENTINEL_B64.as_bytes()] {
                ensure!(
                    !contains(&bytes, needle),
                    "node {n}: plaintext secret found in {}",
                    f.display()
                );
            }
            saw_install |= contains(&bytes, b"hooked-plugin");
        }
        ensure!(
            saw_install,
            "node {n}: the scan never saw the install record"
        );
    }
    drop(procs);
    Ok(())
}
