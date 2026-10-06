//! `--ca-file` (custom CA support) on the real binaries.

use std::time::Duration;

use anyhow::{Context, Result};
use tokio::process::Command;
use wayhouse_fleet_tests::tls_front::{tls_front, TEST_CA};
use wayhouse_fleet_tests::{
    bin_path, build_fleet_bins, free_port, minimal_wayhouse_config, spawn_controller, wait_http_up,
};

const MISSING: &str = "/nonexistent/wayhouse-ca.pem";

/// A flag accepted by clap but never loaded would make `--ca-file` a silent
/// no-op on that binary; a bad file must stop every one of them at startup.
#[tokio::test]
async fn every_binary_rejects_a_missing_ca_file() -> Result<()> {
    build_fleet_bins()?;
    let data = tempfile::tempdir()?;
    let data_dir = data.path().display().to_string();
    let cases: [(&str, Vec<&str>); 5] = [
        ("wayhouse", vec![]),
        ("wayhouse-controller", vec!["--data-dir", &data_dir]),
        ("wayhouse-aggregator", vec![]),
        ("wayhouse-ui", vec![]),
        (
            "wayhouse-agent",
            vec![
                "--data-dir",
                &data_dir,
                "--controller-url",
                "http://127.0.0.1:1",
                "--name",
                "ca-file-test",
            ],
        ),
    ];
    for (bin, extra) in cases {
        let out = tokio::time::timeout(
            Duration::from_secs(30),
            Command::new(bin_path(bin))
                .args(["--ca-file", MISSING])
                .args(&extra)
                .kill_on_drop(true)
                .output(),
        )
        .await
        .with_context(|| format!("{bin} kept running with a missing --ca-file"))??;
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!out.status.success(), "{bin} exited 0:\n{text}");
        assert!(
            text.contains(&format!("--ca-file {MISSING}")),
            "{bin} did not name the file:\n{text}"
        );
    }
    Ok(())
}

/// The docs/12 "behind TLS" pattern with a private CA: a TLS terminator whose
/// certificate the test CA signed, in front of a real `wayhouse-controller`.
#[tokio::test]
async fn wayhouse_check_reaches_a_private_ca_controller() -> Result<()> {
    build_fleet_bins()?;
    let data = tempfile::tempdir()?;
    let port = free_port()?;
    let _controller = spawn_controller(data.path(), port)?;
    let plain = format!("http://127.0.0.1:{port}");
    wait_http_up(&format!("{plain}/healthz"), Duration::from_secs(30)).await?;
    wayhouse_http::client()
        .post(format!("{plain}/config"))
        .body(minimal_wayhouse_config(
            free_port()?,
            free_port()?,
            free_port()?,
        ))
        .send()
        .await?
        .error_for_status()?;

    let (front, _front_task) = tls_front(([127, 0, 0, 1], port).into()).await?;
    let https = format!("https://localhost:{}", front.port());
    let check = |extra: &'static [&'static str]| {
        let https = https.clone();
        async move {
            let out = tokio::time::timeout(
                Duration::from_secs(60),
                Command::new(bin_path("wayhouse"))
                    .args(["--check", "--controller", &https])
                    .args(extra)
                    .kill_on_drop(true)
                    .output(),
            )
            .await??;
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            anyhow::Ok((out.status.success(), text))
        }
    };

    let (ok, text) = check(&[]).await?;
    assert!(!ok, "trusted a private CA without --ca-file:\n{text}");
    let lower = text.to_lowercase();
    assert!(
        lower.contains("certificate") || lower.contains("unknownissuer"),
        "failed, but not on certificate verification:\n{text}"
    );

    let (ok, text) = check(&["--ca-file", TEST_CA]).await?;
    assert!(ok, "wayhouse --check failed with --ca-file:\n{text}");
    Ok(())
}
