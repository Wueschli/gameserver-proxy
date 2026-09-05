//! Phase 13 slice 5 (`docs/08-roadmap.md` "Regional health fabric"):
//! multi-process verification of the Tier-2 gossip mesh. See `src/lib.rs`
//! for why this crate drives real binaries instead of an in-process
//! harness — the point here specifically is a genuinely *mixed*
//! local/domain view, which no single-process test can produce for real
//! (see `gossip_gsp_config`'s doc comment for how the fixture achieves it
//! with real processes and no faked wire data).

use std::io::Write;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::Result;
use gsp_fleet_tests::*;

fn ensure_built() {
    static BUILT: OnceLock<()> = OnceLock::new();
    BUILT.get_or_init(|| build_fleet_bins().expect("building fleet binaries"));
}

const UP: Duration = Duration::from_secs(10);

fn write_config(dir: &tempfile::TempDir, name: &str, yaml: &str) -> Result<std::path::PathBuf> {
    let path = dir.path().join(name);
    let mut f = std::fs::File::create(&path)?;
    f.write_all(yaml.as_bytes())?;
    Ok(path)
}

/// Three real `gsp` processes, one shared `failure_domain`, all three
/// pointed at the same real (unreachable) backend address. A and B run the
/// pool's default `fall: 1` (one failed check flips them locally
/// unhealthy); C runs `fall: 1000` so its *own* checks alone would keep it
/// reporting healthy for a very long time. All three still publish their
/// raw per-check result into the mesh regardless of their own `fall` (see
/// `health.rs::sweep` — a check result is published before `observe`'s
/// rise/fall logic ever runs), so C's vote is real and honest, just not
/// enough on its own to flip its *local* flag.
///
/// This proves docs/10 "Tier 2"'s stated property live, with a genuinely
/// different per-instance local view (not just three votes racing to agree
/// on the same outcome anyway): C's own health check would never have
/// caught this on its own within this test's window, yet its combined
/// `is_healthy()` still goes false, purely from the domain quorum outvoting
/// its own lenient local threshold — "an instance cannot ignore a
/// domain-wide outage" applied to a real separate process, not a unit test.
#[tokio::test]
async fn domain_quorum_overrides_an_instance_with_a_lenient_local_threshold() -> Result<()> {
    ensure_built();
    let dir = tempfile::tempdir()?;

    let target_port = free_port()?; // nothing ever listens here
    let domain = "fleet-test-domain";
    let psk = "fleet-test-psk";
    let quorum_fraction = 0.66;

    let gossip_b = free_port()?;
    let gossip_a = free_port()?;
    let gossip_c = free_port()?;

    let admin_a = free_port()?;
    let admin_b = free_port()?;
    let admin_c = free_port()?;

    // B is the seed everyone else joins through; order of startup doesn't
    // matter beyond that (foca's own membership protocol handles the rest).
    let cfg_b = gossip_gsp_config(
        admin_b,
        free_port()?,
        target_port,
        gossip_b,
        None,
        1,
        quorum_fraction,
        domain,
        psk,
    );
    let cfg_a = gossip_gsp_config(
        admin_a,
        free_port()?,
        target_port,
        gossip_a,
        Some(gossip_b),
        1,
        quorum_fraction,
        domain,
        psk,
    );
    let cfg_c = gossip_gsp_config(
        admin_c,
        free_port()?,
        target_port,
        gossip_c,
        Some(gossip_b),
        1000, // C's own fall threshold: (almost) never trips locally
        quorum_fraction,
        domain,
        psk,
    );

    let path_b = write_config(&dir, "b.yaml", &cfg_b)?;
    let path_a = write_config(&dir, "a.yaml", &cfg_a)?;
    let path_c = write_config(&dir, "c.yaml", &cfg_c)?;

    let _b = spawn_gsp(GspArgs {
        config_path: Some(path_b),
        ..Default::default()
    })?;
    let admin_b_url = format!("http://127.0.0.1:{admin_b}");
    wait_http_up(&format!("{admin_b_url}/healthz"), UP).await?;

    let _a = spawn_gsp(GspArgs {
        config_path: Some(path_a),
        ..Default::default()
    })?;
    let _c = spawn_gsp(GspArgs {
        config_path: Some(path_c),
        ..Default::default()
    })?;
    let admin_a_url = format!("http://127.0.0.1:{admin_a}");
    let admin_c_url = format!("http://127.0.0.1:{admin_c}");
    wait_http_up(&format!("{admin_a_url}/healthz"), UP).await?;
    wait_http_up(&format!("{admin_c_url}/healthz"), UP).await?;

    // Membership: all three converge (each sees the other two).
    for admin in [&admin_a_url, &admin_b_url, &admin_c_url] {
        wait_until(
            || {
                let admin = admin.clone();
                async move {
                    let body = reqwest::get(format!("{admin}/metrics"))
                        .await?
                        .text()
                        .await?;
                    Ok(body.lines().any(|l| l == "gsp_gossip_members 2"))
                }
            },
            UP,
            "gossip mesh to converge to 2 known peers",
        )
        .await?;
    }

    // The interesting assertion: C's combined view goes unhealthy even
    // though its own `fall: 1000` alone could never have done that within
    // this test.
    wait_until(
        || {
            let admin_c_url = admin_c_url.clone();
            async move {
                let body = reqwest::get(format!("{admin_c_url}/pools"))
                    .await?
                    .text()
                    .await?;
                Ok(body.contains("unhealthy"))
            }
        },
        UP,
        "C's combined view to go unhealthy via the domain quorum override",
    )
    .await?;

    // And it's specifically the domain override doing it, not a delayed
    // local flip: the domain-down gauge is set on C.
    let metrics_c = reqwest::get(format!("{admin_c_url}/metrics"))
        .await?
        .text()
        .await?;
    assert!(
        metrics_c
            .lines()
            .any(|l| l.starts_with("gsp_backend_domain_down") && l.trim_end().ends_with(" 1")),
        "expected gsp_backend_domain_down=1 on C, got:\n{metrics_c}"
    );

    Ok(())
}

/// Two real `gsp` processes with *different* PSKs never merge into one
/// mesh (HMAC verification rejects every datagram from the other), and
/// neither one gets stuck or crashes as a result — each just falls back to
/// its own local-only health behaviour, exactly the "fully rebuildable"
/// property docs/10 "Tier 2" calls for on an empty/unreachable mesh.
#[tokio::test]
async fn instances_with_different_psks_never_merge_and_neither_gets_stuck() -> Result<()> {
    ensure_built();
    let dir = tempfile::tempdir()?;

    let target_port = free_port()?;
    let domain = "fleet-test-domain";

    let gossip_a = free_port()?;
    let gossip_b = free_port()?;
    let admin_a = free_port()?;
    let admin_b = free_port()?;

    let cfg_a = gossip_gsp_config(
        admin_a,
        free_port()?,
        target_port,
        gossip_a,
        Some(gossip_b),
        1,
        0.66,
        domain,
        "psk-a",
    );
    let cfg_b = gossip_gsp_config(
        admin_b,
        free_port()?,
        target_port,
        gossip_b,
        None,
        1,
        0.66,
        domain,
        "psk-b",
    );

    let path_a = write_config(&dir, "a.yaml", &cfg_a)?;
    let path_b = write_config(&dir, "b.yaml", &cfg_b)?;

    let _a = spawn_gsp(GspArgs {
        config_path: Some(path_a),
        ..Default::default()
    })?;
    let _b = spawn_gsp(GspArgs {
        config_path: Some(path_b),
        ..Default::default()
    })?;
    let admin_a_url = format!("http://127.0.0.1:{admin_a}");
    let admin_b_url = format!("http://127.0.0.1:{admin_b}");
    wait_http_up(&format!("{admin_a_url}/healthz"), UP).await?;
    wait_http_up(&format!("{admin_b_url}/healthz"), UP).await?;

    // Both still detect the real unreachable backend on their own, purely
    // locally, and both processes stay up and responsive throughout.
    for admin in [&admin_a_url, &admin_b_url] {
        wait_until(
            || {
                let admin = admin.clone();
                async move {
                    let body = reqwest::get(format!("{admin}/pools")).await?.text().await?;
                    Ok(body.contains("unhealthy"))
                }
            },
            UP,
            "backend to go locally unhealthy despite the mesh never forming",
        )
        .await?;
    }

    // Never merged: each only ever knows about itself.
    for admin in [&admin_a_url, &admin_b_url] {
        let body = reqwest::get(format!("{admin}/metrics"))
            .await?
            .text()
            .await?;
        assert!(
            body.lines().any(|l| l == "gsp_gossip_members 0"),
            "expected gsp_gossip_members 0 (mesh never formed across mismatched PSKs), got:\n{body}"
        );
    }

    Ok(())
}
