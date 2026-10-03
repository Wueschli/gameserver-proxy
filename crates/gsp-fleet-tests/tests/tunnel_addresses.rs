//! The controller's address-authority surface over real HTTP (spec: Controller).
//! Plain `cargo test`: no namespaces needed.

use std::time::Duration;

use anyhow::Result;
use gsp_fleet_tests::{build_fleet_bins, free_port, spawn_controller_with, wait_http_up, Proc};
use serde_json::{json, Value};

const KEY: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

async fn controller(extra: &[&str]) -> Result<(Proc, tempfile::TempDir, String)> {
    build_fleet_bins()?;
    let dir = tempfile::tempdir()?;
    let port = free_port()?;
    let extra: Vec<String> = extra.iter().map(|s| s.to_string()).collect();
    let ctl = spawn_controller_with(dir.path(), &format!("127.0.0.1:{port}"), &extra)?;
    wait_http_up(
        &format!("http://127.0.0.1:{port}/healthz"),
        Duration::from_secs(10),
    )
    .await?;
    Ok((ctl, dir, format!("http://127.0.0.1:{port}")))
}

#[tokio::test]
async fn the_controller_allocates_refuses_conflicts_and_lists_the_table() -> Result<()> {
    let (_ctl, _dir, base) = controller(&["--tunnel-network", "10.60.0.0/16"]).await?;
    let http = reqwest::Client::new();

    let r: Value = http
        .post(format!("{base}/peers"))
        .json(&json!({"name": "o1", "pubkey": KEY, "backends": [":25565"]}))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(r["tunnel_address"], "10.60.0.1");
    assert_eq!(r["tunnel_network"], "10.60.0.0/16");

    let conflict = http
        .post(format!("{base}/proxy-peers"))
        .json(
            &json!({"name": "p1", "pubkey": KEY, "endpoint": "203.0.113.9:51820",
                      "tunnel_address": "10.60.0.1"}),
        )
        .send()
        .await?;
    assert_eq!(conflict.status(), 409);
    let body: Value = conflict.json().await?;
    assert!(body["error"].as_str().unwrap().contains("origin \"o1\""));

    let reg: Value = http
        .get(format!("{base}/peers/o1"))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(reg["backends"][0], "10.60.0.1:25565");

    let table: Value = http
        .get(format!("{base}/tunnel/addresses"))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(table["allocated"], 1);
    assert_eq!(table["entries"][0]["name"], "o1");
    Ok(())
}

#[tokio::test]
async fn an_ipv6_tunnel_network_allocates_ipv6_addresses_and_bracketed_backends() -> Result<()> {
    let (_ctl, _dir, base) = controller(&["--tunnel-network", "fd49:89c1:4b5e:60::/64"]).await?;
    let http = reqwest::Client::new();

    let r: Value = http
        .post(format!("{base}/peers"))
        .json(&json!({"name": "o1", "pubkey": KEY, "backends": [":25565"]}))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(r["tunnel_address"], "fd49:89c1:4b5e:60::1");
    assert_eq!(r["tunnel_network"], "fd49:89c1:4b5e:60::/64");

    let reg: Value = http
        .get(format!("{base}/peers/o1"))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(reg["backends"][0], "[fd49:89c1:4b5e:60::1]:25565");

    // An IPv6 underlay endpoint, and a pin of the other family.
    let wrong_family = http
        .post(format!("{base}/proxy-peers"))
        .json(
            &json!({"name": "p1", "pubkey": KEY, "endpoint": "[2001:db8::7]:51820",
                      "tunnel_address": "10.60.0.2"}),
        )
        .send()
        .await?;
    assert_eq!(wrong_family.status(), 422);

    let table: Value = http
        .get(format!("{base}/tunnel/addresses"))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(table["capacity"], 65534);
    assert_eq!(table["network"], "fd49:89c1:4b5e:60::/64");
    Ok(())
}

#[tokio::test]
async fn tunnel_network_together_with_ha_is_refused_at_startup() -> Result<()> {
    build_fleet_bins()?;
    let dir = tempfile::tempdir()?;
    let port = free_port()?;
    let mut ctl = spawn_controller_with(
        dir.path(),
        &format!("127.0.0.1:{port}"),
        &[
            "--tunnel-network".into(),
            "10.60.0.0/16".into(),
            "--ha-node-id".into(),
            "1".into(),
            "--ha-peers".into(),
            "1=127.0.0.1:1".into(),
        ],
    )?;
    gsp_fleet_tests::wait_until(
        || {
            let code = ctl.exit_code();
            async move { Ok(code.is_some_and(|c| c != 0)) }
        },
        Duration::from_secs(10),
        "the controller to exit non-zero",
    )
    .await?;
    assert!(
        ctl.log().contains("--tunnel-network"),
        "the error should name the flag, got: {}",
        ctl.log()
    );
    Ok(())
}

/// Starts a controller on `dir` and waits for it to serve.
async fn controller_in(dir: &std::path::Path, port: u16, extra: &[&str]) -> Result<Proc> {
    let extra: Vec<String> = extra.iter().map(|s| s.to_string()).collect();
    let ctl = spawn_controller_with(dir, &format!("127.0.0.1:{port}"), &extra)?;
    wait_http_up(
        &format!("http://127.0.0.1:{port}/healthz"),
        Duration::from_secs(10),
    )
    .await?;
    Ok(ctl)
}

#[tokio::test]
async fn a_changed_tunnel_network_is_refused_without_touching_the_book() -> Result<()> {
    build_fleet_bins()?;
    let dir = tempfile::tempdir()?;
    let port = free_port()?;
    let base = format!("http://127.0.0.1:{port}");
    let http = reqwest::Client::new();

    let ctl = controller_in(dir.path(), port, &["--tunnel-network", "10.60.0.0/16"]).await?;
    let r: Value = http
        .post(format!("{base}/peers"))
        .json(&json!({"name": "o1", "pubkey": KEY}))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(r["tunnel_address"], "10.60.0.1");
    let before: Value = http
        .get(format!("{base}/tunnel/addresses"))
        .send()
        .await?
        .json()
        .await?;
    drop(ctl);

    let mut refused = spawn_controller_with(
        dir.path(),
        &format!("127.0.0.1:{port}"),
        &["--tunnel-network".into(), "fd49::/64".into()],
    )?;
    gsp_fleet_tests::wait_until(
        || {
            let code = refused.exit_code();
            async move { Ok(code.is_some_and(|c| c != 0)) }
        },
        Duration::from_secs(10),
        "the controller to refuse the changed network",
    )
    .await?;
    let log = refused.log();
    assert!(log.contains("--tunnel-readdress"), "{log}");
    assert!(log.contains("origin o1 10.60.0.1"), "{log}");
    assert!(
        std::net::TcpStream::connect(("127.0.0.1", port)).is_err(),
        "a refused start must not leave a listener"
    );
    drop(refused);

    let ctl = controller_in(dir.path(), port, &["--tunnel-network", "10.60.0.0/16"]).await?;
    let after: Value = http
        .get(format!("{base}/tunnel/addresses"))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(after["entries"], before["entries"], "the book is unchanged");
    drop(ctl);

    // With the flag, it starts and moves o1 at its next registration.
    let _ctl = controller_in(
        dir.path(),
        port,
        &["--tunnel-network", "fd49::/64", "--tunnel-readdress"],
    )
    .await?;
    let r: Value = http
        .post(format!("{base}/peers"))
        .json(&json!({"name": "o1", "pubkey": KEY}))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(r["tunnel_address"], "fd49::1");
    Ok(())
}
