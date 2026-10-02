//! Phase 14 tunnel end-to-end tests — see
//! `docs/superpowers/specs/2026-10-01-tunnel-e2e-design.md`.
//!
//! The namespace tests are `#[ignore]`: they need `CAP_NET_ADMIN` in a
//! network namespace and only run via `make tunnel-e2e` (which wraps the
//! test binary in `unshare -Urnm`). The privilege-free tests at the top run
//! in plain `cargo test`.

use std::io::{Read, Write};
use std::net::SocketAddr;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::Result;
use gsp_fleet_tests::echo::{tcp_roundtrip, udp_roundtrip, EchoServer};
use gsp_fleet_tests::netns::{require_lab_with, Lab};
use gsp_fleet_tests::tunnel::{TunnelLab, PUBLIC_PORT};
use gsp_fleet_tests::{spawn_controller_on, wait_http_up, wait_until};

/// Review Focus 1: outside a lab the failure must say what to do.
#[test]
fn outside_a_lab_the_error_names_the_make_target() {
    let err = require_lab_with(&["false"]).unwrap_err();
    assert!(
        format!("{err:#}").contains("make tunnel-e2e"),
        "error should point at `make tunnel-e2e`, got: {err:#}"
    );
}

/// A veth left behind by an earlier process (each `cargo nextest` test is its
/// own process, so the in-process index restarts at 1 in the same lab, and a dead
/// namespace's veth is torn down asynchronously) must not make `add_ns` fail.
#[test]
#[ignore = "needs a user+net namespace: run via `make tunnel-e2e`"]
fn add_ns_skips_veths_left_by_an_earlier_process() -> Result<()> {
    let mut lab = Lab::new()?;
    // Make indices 1..=24 (more than any run allocates) taken. Earlier tests in
    // this lab may already have left some behind, which is just as stale, so a
    // failed `ip link add` is fine as long as the veth exists afterwards. (The
    // tunnel suite runs one test at a time, so deleting these below is safe.)
    let stale: Vec<String> = (1..=24).map(|i| format!("gv{i}l")).collect();
    for (i, lab_if) in stale.iter().enumerate() {
        let _ = std::process::Command::new("ip")
            .args(["link", "add", lab_if, "type", "veth", "peer", "name"])
            .arg(format!("gv{}n", i + 1))
            .output()?;
        let exists = std::process::Command::new("ip")
            .args(["link", "show", lab_if])
            .output()?
            .status
            .success();
        anyhow::ensure!(exists, "stale veth {lab_if} could not be set up");
    }
    let result = lab.add_ns();
    for lab_if in &stale {
        let _ = std::process::Command::new("ip")
            .args(["link", "del", lab_if])
            .status();
    }
    result.map(|_| ())
}

/// Two namespaces reach each other only through the lab's forwarding.
#[test]
#[ignore = "needs a user+net namespace: run via `make tunnel-e2e`"]
fn namespaces_reach_each_other_through_the_lab() -> Result<()> {
    let mut lab = Lab::new()?;
    let edge = lab.add_ns()?;
    let origin = lab.add_ns()?;
    let origin_ip = origin.underlay();

    let server = origin.spawn_thread(|| -> Result<()> {
        let l = std::net::TcpListener::bind("0.0.0.0:7001")?;
        let (mut s, _) = l.accept()?;
        s.write_all(b"pong")?;
        Ok(())
    })?;

    let got = edge.in_ns(move || -> Result<String> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut s = loop {
            match std::net::TcpStream::connect((origin_ip, 7001)) {
                Ok(s) => break s,
                Err(e) if Instant::now() >= deadline => anyhow::bail!("connect: {e}"),
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        };
        let mut buf = String::new();
        s.read_to_string(&mut buf)?;
        Ok(buf)
    })??;
    assert_eq!(got, "pong");
    server.join().expect("server thread")??;
    Ok(())
}

/// Review Focus 2: a dropped `Ns` must not leave its holder process behind.
#[test]
#[ignore = "needs a user+net namespace: run via `make tunnel-e2e`"]
fn dropping_a_namespace_reaps_its_holder() -> Result<()> {
    let mut lab = Lab::new()?;
    let ns = lab.add_ns()?;
    let proc_dir = format!("/proc/{}", ns.pid());
    assert!(std::path::Path::new(&proc_dir).exists());
    drop(ns);
    assert!(
        !std::path::Path::new(&proc_dir).exists(),
        "holder process {proc_dir} survived the drop"
    );
    Ok(())
}

/// The echo server lives *inside* the origin namespace; the client in the lab
/// reaches it across the veth. Large TCP + a MTU-sized UDP datagram.
#[tokio::test]
#[ignore = "needs a user+net namespace: run via `make tunnel-e2e`"]
async fn echo_server_round_trips_tcp_and_udp_and_stops_on_drop() -> Result<()> {
    let mut lab = Lab::new()?;
    let origin = lab.add_ns()?;
    let addr = SocketAddr::new(origin.underlay().into(), 7000);

    let echo = EchoServer::start(&origin, 7000)?;
    let big: Vec<u8> = (0..256 * 1024).map(|i| (i % 251) as u8).collect();
    assert_eq!(tcp_roundtrip(addr, &big).await?, big);
    let dgram = vec![0x5a; 1200];
    assert_eq!(udp_roundtrip(addr, &dgram).await?, dgram);

    drop(echo);
    let err = tcp_roundtrip(addr, b"x").await.unwrap_err();
    assert!(
        format!("{err:#}").to_lowercase().contains("refused"),
        "after drop the port should refuse connections, got: {err:#}"
    );
    Ok(())
}

/// Logs are captured (existing helpers discard them) so a failing scenario can
/// show what each process said. Runs in plain `cargo test`: no namespaces.
#[tokio::test]
async fn proc_captures_output_for_failure_reports() -> Result<()> {
    gsp_fleet_tests::build_fleet_bins()?;
    let dir = tempfile::tempdir()?;
    let port = gsp_fleet_tests::free_port()?;
    let ctl = spawn_controller_on(dir.path(), &format!("127.0.0.1:{port}"))?;
    wait_http_up(
        &format!("http://127.0.0.1:{port}/healthz"),
        Duration::from_secs(10),
    )
    .await?;
    assert_eq!(ctl.name(), "gsp-controller");
    // The controller logs at startup; the log file must have something in it.
    gsp_fleet_tests::wait_until(
        || {
            let log = ctl.log();
            async move { Ok(!log.is_empty()) }
        },
        Duration::from_secs(5),
        "controller output to be captured",
    )
    .await?;
    Ok(())
}

fn ensure_built() {
    static BUILT: OnceLock<()> = OnceLock::new();
    BUILT.get_or_init(|| gsp_fleet_tests::build_fleet_bins().expect("building binaries"));
}

/// Scenario 1. Real `gsp-controller` + `gsp-agent` + `gsp --tunnel-*`, real
/// WireGuard, the echo server started LAST (Review Focus 4: the backend is
/// first seen unhealthy and must recover on its own), then payloads that cross
/// the WireGuard MTU (Review Focus 3).
#[tokio::test]
#[ignore = "needs a user+net namespace: run via `make tunnel-e2e`"]
async fn tcp_and_udp_round_trip_through_the_tunnel() -> Result<()> {
    ensure_built();
    let mut t = TunnelLab::new().await?;
    t.start_origin(false).await?; // agent up, nothing listening on :7000 yet
    let edge = t.start_edge("edge-1", None).await?;

    // Tunnel up first (via a probe port outside the pool), so the unhealthy
    // state below can only be because nothing listens on :7000 — not because a
    // handshake is still pending (userspace's takes ~25 s).
    t.wait_tunnel_up(edge).await?;
    t.wait_backends(edge, false).await?; // nothing answers on :7000 -> unhealthy
    t.start_echo()?;
    t.wait_backends(edge, true).await?; // recovers with no restart
    t.wait_roundtrip(edge).await?; // proves the tunnel, not just /pools

    let public = t.public_addr(edge);
    let big: Vec<u8> = (0..256 * 1024).map(|i| (i % 251) as u8).collect();
    assert_eq!(tcp_roundtrip(public, &big).await?, big, "256 KiB over TCP");
    let dgram = vec![0xa5; 1200];
    assert_eq!(
        udp_roundtrip(public, &dgram).await?,
        dgram,
        "1200 B over UDP"
    );
    assert_eq!(public.port(), PUBLIC_PORT);
    t.pass();
    Ok(())
}

/// Scenario 2 — the slice-6 regression. Both sides re-register every 1 s
/// (set by `TunnelLab`). Before the fix, every unchanged re-registration tore
/// down and rebuilt the WireGuard session, so the handshake never stabilised.
/// After the first success, every probe for the next ~12 s must succeed.
#[tokio::test]
#[ignore = "needs a user+net namespace: run via `make tunnel-e2e`"]
async fn tunnel_stays_up_across_many_re_registrations() -> Result<()> {
    ensure_built();
    let mut t = TunnelLab::new().await?;
    t.start_origin(true).await?;
    let edge = t.start_edge("edge-1", None).await?;
    t.wait_roundtrip(edge).await?;

    let public = t.public_addr(edge);
    let started = Instant::now();
    let mut probes = 0u32;
    while started.elapsed() < Duration::from_secs(12) {
        let got = tcp_roundtrip(public, b"stable?").await?;
        assert_eq!(got, b"stable?", "probe {probes} came back wrong");
        probes += 1;
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(probes >= 30, "only {probes} probes ran in 12s");
    assert!(t.agent_alive(), "agent exited during the run");
    t.pass();
    Ok(())
}

/// Scenario 3 — slice 7, never before verified live. An origin is already up
/// with one proxy; a *second* proxy joins later. The agent is never restarted
/// or reconfigured: it must learn the new proxy from the proxy-peers registry,
/// and traffic must flow through the new proxy.
#[tokio::test]
#[ignore = "needs a user+net namespace: run via `make tunnel-e2e`"]
async fn a_proxy_added_later_is_learned_without_restarting_the_agent() -> Result<()> {
    ensure_built();
    let mut t = TunnelLab::new().await?;
    t.start_origin(true).await?;
    let first = t.start_edge("edge-1", None).await?;
    t.wait_roundtrip(first).await?;

    // New proxy: own namespace, own tunnel address and key. Nothing about
    // the origin is touched.
    let second = t.start_edge("edge-2", None).await?;
    t.wait_roundtrip(second).await?;

    let big = vec![0x42u8; 64 * 1024];
    assert_eq!(tcp_roundtrip(t.public_addr(second), &big).await?, big);
    assert!(
        t.agent_alive(),
        "the agent must not have been restarted or crashed"
    );
    t.pass();
    Ok(())
}

/// Scenario 5 — the multi-proxy fix. Two proxies share one origin and both
/// must carry traffic at the same time. Before the address authority the agent
/// gave every proxy `AllowedIPs = 0.0.0.0/0`, so the proxy that registered last
/// stole the earlier one's route and its connections timed out.
#[tokio::test]
#[ignore = "needs a user+net namespace: run via `make tunnel-e2e`"]
async fn two_proxies_share_one_origin() -> Result<()> {
    ensure_built();
    let mut t = TunnelLab::new().await?;
    t.start_origin(true).await?;
    let first = t.start_edge("edge-1", None).await?;
    t.wait_roundtrip(first).await?;
    let second = t.start_edge("edge-2", None).await?;
    t.wait_roundtrip(second).await?;

    // Both proxies must carry traffic at the same time.
    assert_eq!(
        tcp_roundtrip(t.public_addr(second), b"second").await?,
        b"second"
    );
    assert_eq!(
        tcp_roundtrip(t.public_addr(first), b"first").await?,
        b"first",
        "the first proxy lost its route when the second registered"
    );
    t.pass();
    Ok(())
}

/// Scenario 4 — Review Focus 5. `gsp`'s tunnel source pins the origin's
/// pubkey; if the registry's key for that name differs, `fetch` must refuse
/// ("a key change is refused loudly"), so the pool stays empty and nothing
/// reaches the backend.
#[tokio::test]
#[ignore = "needs a user+net namespace: run via `make tunnel-e2e`"]
async fn a_pinned_key_that_does_not_match_the_registry_is_refused() -> Result<()> {
    ensure_built();
    let mut t = TunnelLab::new().await?;
    t.start_origin(true).await?;
    // 32 zero bytes, base64 — a valid key, just not the origin's.
    let wrong = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
    assert_ne!(t.origin_pubkey(), wrong);
    let edge = t.start_edge("edge-bad", Some(wrong)).await?;

    // Bounded negative window: the source refreshes every 1 s, so 8 s is
    // several refresh cycles — long enough that "never got a backend" means
    // something. This is the one deliberate fixed wait in the suite.
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        let pools = t.pools(edge).await?;
        assert!(
            !pools.lines().any(|l| l.starts_with("  ")),
            "a mismatched key must never yield a backend, but /pools shows:\n{pools}"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    let attempt = tcp_roundtrip(t.public_addr(edge), b"nope").await;
    assert!(attempt.is_err(), "traffic must not flow, got {attempt:?}");
    t.pass();
    Ok(())
}

/// Scenario 6 — a hand-picked address another peer already holds is refused
/// with a clear error, and the holder is unaffected.
#[tokio::test]
#[ignore = "needs a user+net namespace: run via `make tunnel-e2e`"]
async fn a_pinned_address_collision_is_refused() -> Result<()> {
    ensure_built();
    let mut t = TunnelLab::new().await?;
    t.start_origin(true).await?;
    let taken = t.origin_ip();
    let log = t.agent_refused("origin-b", &format!("{taken}/16")).await?;
    assert!(
        log.contains("already held"),
        "the refusal should say why, got:\n{log}"
    );
    assert!(t.agent_alive(), "the first origin must be unaffected");
    t.pass();
    Ok(())
}

/// Scenario 7 — an edge that restarts keeps its tunnel address (the
/// controller's allocation is sticky) and traffic recovers.
#[tokio::test]
#[ignore = "needs a user+net namespace: run via `make tunnel-e2e`"]
async fn an_edge_restart_keeps_its_address() -> Result<()> {
    ensure_built();
    let mut t = TunnelLab::new().await?;
    t.start_origin(true).await?;
    let edge = t.start_edge("edge-1", None).await?;
    t.wait_roundtrip(edge).await?;
    let before = t.proxy_address("edge-1").await?;

    t.restart_edge(edge).await?;
    t.wait_roundtrip_after_restart(edge).await?;
    assert_eq!(t.proxy_address("edge-1").await?, before);
    t.pass();
    Ok(())
}

/// Scenario 8 — Review Focus 5. An edge restarts while the controller is down:
/// it must come up on its saved address (admin `/healthz` answers), and when
/// the controller returns it re-registers with the same address.
#[tokio::test]
#[ignore = "needs a user+net namespace: run via `make tunnel-e2e`"]
async fn an_edge_restarts_with_the_controller_down() -> Result<()> {
    ensure_built();
    let mut t = TunnelLab::new().await?;
    t.start_origin(true).await?;
    let edge = t.start_edge("edge-1", None).await?;
    t.wait_roundtrip(edge).await?;
    let before = t.proxy_address("edge-1").await?;
    let seen_before = t.proxy_last_seen("edge-1").await?;

    t.stop_controller().await?;
    // `restart_edge` returns only once the admin API answers — i.e. startup
    // went ahead on the saved address instead of failing.
    t.restart_edge(edge).await?;
    t.start_controller().await?;

    {
        let t = &t;
        wait_until(
            || async move {
                Ok(t.proxy_last_seen("edge-1")
                    .await
                    .is_ok_and(|s| s > seen_before))
            },
            Duration::from_secs(30),
            "the restarted edge to re-register with the returned controller",
        )
        .await?;
    }
    assert_eq!(t.proxy_address("edge-1").await?, before);
    t.pass();
    Ok(())
}
