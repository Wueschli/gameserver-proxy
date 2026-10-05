//! Startup refusals for unauthenticated or weakly protected APIs
//! (docs/security-review-2026-10.md N1, O1, O7).

use std::time::Duration;

use anyhow::{ensure, Result};
use gsp_fleet_tests::{bin_path, build_fleet_bins, free_port};
use tokio::process::Command;

/// Run `bin` with `args` and require a non-zero exit whose stderr contains `needle`.
async fn refused(bin: &str, args: &[&str], needle: &str) -> Result<()> {
    build_fleet_bins()?;
    let out = tokio::time::timeout(
        Duration::from_secs(30),
        Command::new(bin_path(bin))
            .args(args)
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    let text = String::from_utf8_lossy(&out.stderr);
    ensure!(!out.status.success(), "{bin} {args:?} started:\n{text}");
    ensure!(
        text.contains(needle),
        "{bin} {args:?}: no {needle:?} in:\n{text}"
    );
    Ok(())
}

#[tokio::test]
async fn open_apis_refuse_a_non_loopback_bind() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let data = dir.path().to_str().unwrap();
    let listen = format!("0.0.0.0:{}", free_port()?);
    refused(
        "gsp-controller",
        &["--listen", &listen, "--data-dir", data],
        "--insecure-no-auth",
    )
    .await?;
    refused(
        "gsp-aggregator",
        &["--listen", &listen],
        "--insecure-no-auth",
    )
    .await?;
    refused("gsp-ui", &["--listen", &listen], "--insecure-no-auth").await
}

#[tokio::test]
async fn ha_without_a_token_is_refused() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let data = dir.path().to_str().unwrap();
    let listen = format!("127.0.0.1:{}", free_port()?);
    refused(
        "gsp-controller",
        &[
            "--listen",
            &listen,
            "--data-dir",
            data,
            "--ha-node-id",
            "1",
            "--ha-peers",
            "1=127.0.0.1:1",
        ],
        "--ha-token",
    )
    .await?;
    refused(
        "gsp-controller",
        &[
            "--listen",
            &listen,
            "--data-dir",
            data,
            "--ha-node-id",
            "4",
            "--ha-join",
        ],
        "--ha-token",
    )
    .await
}

#[tokio::test]
async fn short_tokens_are_refused() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let data = dir.path().to_str().unwrap();
    let listen = format!("127.0.0.1:{}", free_port()?);
    refused(
        "gsp-controller",
        &[
            "--listen",
            &listen,
            "--data-dir",
            data,
            "--auth-token",
            "short",
        ],
        "too short",
    )
    .await?;
    refused(
        "gsp-controller",
        &[
            "--listen",
            &listen,
            "--data-dir",
            data,
            "--ha-node-id",
            "1",
            "--ha-peers",
            "1=127.0.0.1:1",
            "--ha-token",
            "short",
        ],
        "too short",
    )
    .await?;
    refused(
        "gsp-aggregator",
        &["--listen", &listen, "--auth-token", "short"],
        "too short",
    )
    .await
}

/// An aggregator with an admin token must have a separate ingest token, or the
/// proxies that push would hold the token that drives the fleet.
#[tokio::test]
async fn an_aggregator_auth_token_requires_an_ingest_token() -> Result<()> {
    let listen = format!("127.0.0.1:{}", free_port()?);
    let auth = "admin-token-0123456789";
    refused(
        "gsp-aggregator",
        &["--listen", &listen, "--auth-token", auth],
        "requires --ingest-token",
    )
    .await?;
    refused(
        "gsp-aggregator",
        &[
            "--listen",
            &listen,
            "--auth-token",
            auth,
            "--ingest-token",
            auth,
        ],
        "must differ",
    )
    .await
}
