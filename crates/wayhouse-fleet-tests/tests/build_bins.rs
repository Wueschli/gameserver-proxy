//! `build_tunnel_bins` (what the lab runs inside the namespace after
//! `make tunnel-e2e` already built the binaries) must be a no-op on a built
//! tree. It runs under `cargo test`, which sets `CARGO_PKG_*` and friends; a
//! child `cargo build` that inherits them makes `ring`'s build script (which
//! declares them with `rerun-if-env-changed`) rerun and recompile `ring` on
//! every call (issue #67).

use std::process::Command;

use anyhow::{ensure, Result};

const PKGS: [&str; 6] = [
    "-p",
    "wayhouse",
    "-p",
    "wayhouse-controller",
    "-p",
    "wayhouse-agent",
];

#[test]
fn the_tunnel_build_is_a_no_op_on_a_built_tree() -> Result<()> {
    // Warm up as a shell would run it: none of cargo's per-test variables.
    let mut warm = Command::new("cargo");
    warm.arg("build")
        .args(PKGS)
        .args(wayhouse_fleet_tests::PROTOCOL_OVERRIDE_FEATURE);
    for (k, _) in std::env::vars() {
        if k.starts_with("CARGO_PKG_")
            || k.starts_with("CARGO_MANIFEST_")
            || k == "CARGO_CRATE_NAME"
        {
            warm.env_remove(k);
        }
    }
    ensure!(warm.status()?.success(), "warm-up build failed");

    let mut build = wayhouse_fleet_tests::tunnel_bins_build_command();
    let out = build.output()?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    ensure!(out.status.success(), "build failed:\n{stderr}");
    ensure!(
        !stderr.contains("Compiling"),
        "a built tree was rebuilt:\n{stderr}"
    );
    Ok(())
}
