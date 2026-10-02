//! `--ca-file` (custom CA support) on the real binaries.

use std::time::Duration;

use anyhow::{Context, Result};
use gsp_fleet_tests::{bin_path, build_fleet_bins};
use tokio::process::Command;

const MISSING: &str = "/nonexistent/gsp-ca.pem";

/// A flag accepted by clap but never loaded would make `--ca-file` a silent
/// no-op on that binary; a bad file must stop every one of them at startup.
#[tokio::test]
async fn every_binary_rejects_a_missing_ca_file() -> Result<()> {
    build_fleet_bins()?;
    let data = tempfile::tempdir()?;
    let data_dir = data.path().display().to_string();
    let cases: [(&str, Vec<&str>); 5] = [
        ("gsp", vec![]),
        ("gsp-controller", vec!["--data-dir", &data_dir]),
        ("gsp-aggregator", vec![]),
        ("gsp-ui", vec![]),
        (
            "gsp-agent",
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
