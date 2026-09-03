//! End-to-end UDP: datagrams flow client -> proxy -> backend -> client, with
//! per-client sessions, `src_ip` backend affinity, and idle-timeout eviction
//! that releases the backend slot.

use std::time::Duration;

use tokio::net::UdpSocket;

use gsp_config::parse_str;
use gsp_core::{Runtime, Snapshot};

/// Spawn a UDP echo server that prefixes every reply with `tag` so the client
/// can tell which backend answered. Returns its address.
async fn echo_backend(tag: u8) -> std::net::SocketAddr {
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = sock.local_addr().unwrap();
    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        while let Ok((n, peer)) = sock.recv_from(&mut buf).await {
            let mut out = vec![tag];
            out.extend_from_slice(&buf[..n]);
            let _ = sock.send_to(&out, peer).await;
        }
    });
    addr
}

fn free_udp_addr() -> std::net::SocketAddr {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

#[tokio::test]
async fn forwards_udp_datagrams_and_reuses_the_session() {
    let b1 = echo_backend(1).await;
    let b2 = echo_backend(2).await;
    let proxy_addr = free_udp_addr();

    let yaml = format!(
        r#"
pools:
  - name: p
    targets: ["{b1}", "{b2}"]
    balancer: round_robin
listeners:
  - name: l
    bind: "{proxy_addr}"
    protocol: udp
    pool: p
"#
    );
    let cfg = parse_str(&yaml).unwrap();
    let runtime = Runtime::start(Snapshot::from_config(&cfg), Default::default(), 1);
    tokio::time::sleep(Duration::from_millis(150)).await;

    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client.connect(proxy_addr).await.unwrap();

    let mut first_tag = None;
    for i in 0..5u8 {
        client.send(&[b'p', b'i', b'n', b'g', i]).await.unwrap();
        let mut buf = [0u8; 32];
        let n = tokio::time::timeout(Duration::from_millis(500), client.recv(&mut buf))
            .await
            .expect("reply timed out")
            .unwrap();
        assert_eq!(&buf[1..n], &[b'p', b'i', b'n', b'g', i]);
        let tag = buf[0];
        match first_tag {
            None => first_tag = Some(tag),
            Some(t) => assert_eq!(tag, t, "same client must stick to the same backend"),
        }
    }

    runtime.shutdown().await;
}

#[tokio::test]
async fn idle_timeout_evicts_the_session_and_frees_the_backend_slot() {
    let backend = echo_backend(7).await;
    let proxy_addr = free_udp_addr();

    // idle_timeout 1s, one backend capped at a single session: a second client
    // can only get through once the first session has been evicted.
    let yaml = format!(
        r#"
pools:
  - name: p
    targets: ["{backend}"]
    idle_timeout_sec: 1
    per_backend: {{ max_sessions: 1 }}
listeners:
  - name: l
    bind: "{proxy_addr}"
    protocol: udp
    pool: p
"#
    );
    let cfg = parse_str(&yaml).unwrap();
    let runtime = Runtime::start(Snapshot::from_config(&cfg), Default::default(), 1);
    tokio::time::sleep(Duration::from_millis(150)).await;

    let roundtrip = |src_port_marker: &'static [u8]| async move {
        let c = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        c.connect(proxy_addr).await.unwrap();
        c.send(src_port_marker).await.unwrap();
        let mut buf = [0u8; 32];
        let r = tokio::time::timeout(Duration::from_millis(500), c.recv(&mut buf)).await;
        r.map(|res| res.unwrap()).map(|n| buf[..n].to_vec())
    };

    let a = roundtrip(b"a").await.expect("first session should work");
    assert_eq!(a, vec![7, b'a']);

    // While A is still alive the capped backend rejects B.
    assert!(
        roundtrip(b"b").await.is_err(),
        "second session must be refused while the first holds the only slot"
    );

    // Wait past the idle timeout + one sweep, then B should succeed.
    tokio::time::sleep(Duration::from_millis(2200)).await;
    let c = roundtrip(b"c")
        .await
        .expect("slot should be free after eviction");
    assert_eq!(c, vec![7, b'c']);

    runtime.shutdown().await;
}

#[tokio::test]
async fn first_bytes_prefix_routes_to_its_pool() {
    let query = echo_backend(b'Q').await;
    let game = echo_backend(b'G').await;
    let proxy_addr = free_udp_addr();

    let yaml = format!(
        r#"
pools:
  - name: query
    targets: ["{query}"]
  - name: game
    targets: ["{game}"]
listeners:
  - name: l
    bind: "{proxy_addr}"
    protocol: udp
    routes:
      - match: {{ type: first_bytes, prefix: "hex:ffffffff" }}
        action: {{ pool: query }}
      - match: {{ type: always }}
        action: {{ pool: game }}
"#
    );
    let cfg = parse_str(&yaml).unwrap();
    let runtime = Runtime::start(Snapshot::from_config(&cfg), Default::default(), 1);
    tokio::time::sleep(Duration::from_millis(150)).await;

    let recv_tag = |payload: &'static [u8]| async move {
        let c = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        c.connect(proxy_addr).await.unwrap();
        c.send(payload).await.unwrap();
        let mut buf = [0u8; 64];
        tokio::time::timeout(Duration::from_millis(500), c.recv(&mut buf))
            .await
            .expect("reply timed out")
            .unwrap();
        buf[0]
    };

    assert_eq!(recv_tag(&[0xff, 0xff, 0xff, 0xff, 0x54, 0x53]).await, b'Q');
    assert_eq!(recv_tag(b"\x01\x02plain gameplay").await, b'G');

    runtime.shutdown().await;
}

#[tokio::test]
async fn first_bytes_length_routes_short_vs_long_datagrams() {
    let short = echo_backend(b'S').await;
    let long = echo_backend(b'L').await;
    let proxy_addr = free_udp_addr();

    let yaml = format!(
        r#"
pools:
  - name: short
    targets: ["{short}"]
  - name: long
    targets: ["{long}"]
listeners:
  - name: l
    bind: "{proxy_addr}"
    protocol: udp
    routes:
      - match: {{ type: first_bytes, length: {{ min: 0, max: 15 }} }}
        action: {{ pool: short }}
      - match: {{ type: always }}
        action: {{ pool: long }}
"#
    );
    let cfg = parse_str(&yaml).unwrap();
    let runtime = Runtime::start(Snapshot::from_config(&cfg), Default::default(), 1);
    tokio::time::sleep(Duration::from_millis(150)).await;

    let tag = |bytes: Vec<u8>| async move {
        let c = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        c.connect(proxy_addr).await.unwrap();
        c.send(&bytes).await.unwrap();
        let mut buf = [0u8; 64];
        tokio::time::timeout(Duration::from_millis(500), c.recv(&mut buf))
            .await
            .expect("reply timed out")
            .unwrap();
        buf[0]
    };

    assert_eq!(tag(vec![1, 2, 3, 4]).await, b'S');
    assert_eq!(tag(vec![0u8; 40]).await, b'L');

    runtime.shutdown().await;
}

#[tokio::test]
async fn udp_prefix_listener_routes_by_destination_ip_and_replies_from_it() {
    let a = echo_backend(b'A').await;
    let b = echo_backend(b'B').await;
    // Grab a free port, then bind the proxy on the wildcard address.
    let port = free_udp_addr().port();

    let yaml = format!(
        r#"
pools:
  - name: a
    targets: ["{a}"]
  - name: b
    targets: ["{b}"]
listeners:
  - name: l
    bind: "0.0.0.0:{port}"
    protocol: udp
    prefix: "127.0.0.0/8"
    routes:
      - match: {{ type: dst, cidrs: ["127.0.0.2/32"] }}
        action: {{ pool: a }}
      - match: {{ type: dst, cidrs: ["127.0.0.3/32"] }}
        action: {{ pool: b }}
      - match: {{ type: always }}
        action: {{ pool: a }}
"#
    );
    let cfg = parse_str(&yaml).unwrap();
    let runtime = Runtime::start(Snapshot::from_config(&cfg), Default::default(), 1);
    tokio::time::sleep(Duration::from_millis(150)).await;

    // The client `connect`s to the sub-address, so it only accepts a reply whose
    // source is exactly that address — proving the sendmsg pktinfo source.
    let hit = |dst: &'static str| async move {
        let c = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        c.connect(format!("{dst}:{port}")).await.unwrap();
        c.send(b"ping").await.unwrap();
        let mut buf = [0u8; 32];
        let n = tokio::time::timeout(Duration::from_millis(500), c.recv(&mut buf))
            .await
            .expect("no reply (wrong reply source address?)")
            .unwrap();
        buf[..n].to_vec()
    };

    assert_eq!(hit("127.0.0.2").await, b"Aping");
    assert_eq!(hit("127.0.0.3").await, b"Bping");

    runtime.shutdown().await;
}
