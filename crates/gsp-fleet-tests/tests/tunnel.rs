//! Phase 14 tunnel end-to-end tests — see
//! `docs/superpowers/specs/2026-10-01-tunnel-e2e-design.md`.
//!
//! The namespace tests are `#[ignore]`: they need `CAP_NET_ADMIN` in a
//! network namespace and only run via `make tunnel-e2e` (which wraps the
//! test binary in `unshare -Urnm`). The privilege-free tests at the top run
//! in plain `cargo test`.

use std::io::{Read, Write};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use anyhow::Result;
use gsp_fleet_tests::echo::{tcp_roundtrip, udp_roundtrip, EchoServer};
use gsp_fleet_tests::netns::{require_lab_with, Lab};

/// Review Focus 1: outside a lab the failure must say what to do.
#[test]
fn outside_a_lab_the_error_names_the_make_target() {
    let err = require_lab_with(&["false"]).unwrap_err();
    assert!(
        format!("{err:#}").contains("make tunnel-e2e"),
        "error should point at `make tunnel-e2e`, got: {err:#}"
    );
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
