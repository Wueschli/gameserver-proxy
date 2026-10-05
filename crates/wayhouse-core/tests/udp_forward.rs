//! End-to-end UDP: datagrams flow client -> proxy -> backend -> client, with
//! per-client sessions, `consistent_hash` backend affinity, and idle-timeout eviction
//! that releases the backend slot.

use std::time::Duration;

use tokio::net::UdpSocket;

use wayhouse_config::parse_str;
use wayhouse_core::{Runtime, Snapshot};

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

/// Send `payload` on `sock` until the echo comes back, for the first datagram
/// on a fresh path. A fixed sleep after `Runtime::start` loses to listener
/// startup under load: the datagram then lands on a port nobody reads yet and
/// is dropped (or bounces as `ConnectionRefused`), so a bare send/recv would
/// time out. Returns the reply length.
async fn first_reply(sock: &UdpSocket, payload: &[u8], buf: &mut [u8]) -> usize {
    for _ in 0..40 {
        // `send` can fail with ConnectionRefused from an earlier ICMP bounce.
        if sock.send(payload).await.is_ok() {
            if let Ok(Ok(n)) =
                tokio::time::timeout(Duration::from_millis(250), sock.recv(buf)).await
            {
                return n;
            }
        } else {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    panic!("the listener never answered the first datagram");
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
    health_check:
      type: none
    balancer: round_robin
listeners:
  - name: l
    bind: "{proxy_addr}"
    protocol: udp
    pool: p
"#
    );
    let cfg = parse_str(&yaml).unwrap();
    let runtime = Runtime::start(Snapshot::from_config(&cfg), std::sync::Arc::default(), 1);
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

    runtime
        .shutdown_with_grace(std::time::Duration::from_millis(100))
        .await;
}

#[tokio::test]
#[allow(clippy::items_after_statements)] // test-local items sit next to their only use
async fn forwards_a_burst_of_datagrams_that_land_in_one_recvmmsg() {
    let b1 = echo_backend(1).await;
    let proxy_addr = free_udp_addr();
    let yaml = format!(
        r#"
pools:
  - name: p
    targets: ["{b1}"]
    health_check:
      type: none
listeners:
  - name: l
    bind: "{proxy_addr}"
    protocol: udp
    pool: p
"#
    );
    let cfg = parse_str(&yaml).unwrap();
    let runtime = Runtime::start(Snapshot::from_config(&cfg), std::sync::Arc::default(), 1);
    tokio::time::sleep(Duration::from_millis(150)).await;

    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client.connect(proxy_addr).await.unwrap();

    // Fire a tight burst with no reads in between, so several datagrams are
    // queued on the listen socket and pulled by a single `recvmmsg`.
    const N: u8 = 40;
    for i in 0..N {
        client.send(&[b'x', i]).await.unwrap();
    }

    // Every datagram must come back exactly once (order per session is FIFO).
    let mut seen = vec![0u32; N as usize];
    let mut buf = [0u8; 32];
    for _ in 0..N {
        let n = tokio::time::timeout(Duration::from_secs(2), client.recv(&mut buf))
            .await
            .expect("reply timed out")
            .unwrap();
        assert_eq!(n, 3, "echo backend prefixes one tag byte");
        assert_eq!(buf[0], 1);
        assert_eq!(buf[1], b'x');
        seen[buf[2] as usize] += 1;
    }
    assert!(
        seen.iter().all(|&c| c == 1),
        "each datagram echoed exactly once: {seen:?}"
    );

    runtime
        .shutdown_with_grace(std::time::Duration::from_millis(100))
        .await;
}

/// Fire `n` numbered datagrams at `proxy` with no reads in between and assert
/// they come back as the echo of each, exactly once and in order: the forward
/// and reply legs both batch (`sendmmsg`), which must not reorder, drop or
/// duplicate within a session.
async fn assert_ordered_echo_burst(client: &UdpSocket, n: u16, tag: u8) {
    for i in 0..n {
        // Varying sizes so a batch mixes lengths.
        let mut payload = i.to_be_bytes().to_vec();
        payload.resize(2 + usize::from(i % 7) * 37, b'z');
        client.send(&payload).await.unwrap();
    }
    let mut buf = [0u8; 512];
    for i in 0..n {
        let len = tokio::time::timeout(Duration::from_secs(2), client.recv(&mut buf))
            .await
            .unwrap_or_else(|_| panic!("reply {i} timed out"))
            .unwrap();
        assert_eq!(buf[0], tag);
        assert_eq!(&buf[1..3], &i.to_be_bytes(), "reply {i} out of order");
        assert_eq!(len, 3 + usize::from(i % 7) * 37, "reply {i} length");
    }
}

#[tokio::test]
async fn a_burst_through_the_batched_forward_and_reply_paths_keeps_order() {
    let b1 = echo_backend(1).await;
    let proxy_addr = free_udp_addr();
    let yaml = format!(
        r#"
pools:
  - name: p
    targets: ["{b1}"]
    health_check:
      type: none
listeners:
  - name: l
    bind: "{proxy_addr}"
    protocol: udp
    pool: p
"#
    );
    let cfg = parse_str(&yaml).unwrap();
    let runtime = Runtime::start(Snapshot::from_config(&cfg), std::sync::Arc::default(), 1);
    tokio::time::sleep(Duration::from_millis(150)).await;

    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client.connect(proxy_addr).await.unwrap();
    // Establish the session first so the burst takes the existing-session path.
    let mut buf = [0u8; 64];
    first_reply(&client, b"hi", &mut buf).await;
    for _ in 0..3 {
        assert_ordered_echo_burst(&client, 100, 1).await;
    }

    runtime
        .shutdown_with_grace(std::time::Duration::from_millis(100))
        .await;
}

#[tokio::test]
async fn a_prefix_mode_burst_replies_in_order_from_the_destination_address() {
    let a = echo_backend(b'A').await;
    let port = free_udp_addr().port();
    let yaml = format!(
        r#"
pools:
  - name: a
    targets: ["{a}"]
    health_check:
      type: none
listeners:
  - name: l
    bind: "0.0.0.0:{port}"
    protocol: udp
    prefix: "127.0.0.0/8"
    pool: a
"#
    );
    let cfg = parse_str(&yaml).unwrap();
    let runtime = Runtime::start(Snapshot::from_config(&cfg), std::sync::Arc::default(), 1);
    tokio::time::sleep(Duration::from_millis(150)).await;

    // `connect`ed to the sub-address: only replies sourced from it are accepted.
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client.connect(format!("127.0.0.2:{port}")).await.unwrap();
    let mut buf = [0u8; 64];
    first_reply(&client, b"hi", &mut buf).await;
    assert_ordered_echo_burst(&client, 100, b'A').await;

    runtime
        .shutdown_with_grace(std::time::Duration::from_millis(100))
        .await;
}

#[tokio::test]
async fn sessions_registry_lists_a_live_udp_session_with_its_pool_and_backend() {
    let backend = echo_backend(9).await;
    let proxy_addr = free_udp_addr();
    let yaml = format!(
        r#"
pools:
  - name: p
    targets: ["{backend}"]
    health_check:
      type: none
listeners:
  - name: l
    bind: "{proxy_addr}"
    protocol: udp
    pool: p
"#
    );
    let cfg = parse_str(&yaml).unwrap();
    let runtime = Runtime::start(Snapshot::from_config(&cfg), std::sync::Arc::default(), 1);
    let handle = runtime.handle();
    tokio::time::sleep(Duration::from_millis(150)).await;

    assert!(handle.sessions().is_empty());

    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client.connect(proxy_addr).await.unwrap();
    client.send(b"hi").await.unwrap();
    let mut buf = [0u8; 32];
    tokio::time::timeout(Duration::from_millis(500), client.recv(&mut buf))
        .await
        .expect("reply timed out")
        .unwrap();

    let live = handle.sessions();
    assert_eq!(live.len(), 1, "one live UDP session expected");
    let e = &live[0];
    assert_eq!(e.proto, wayhouse_core::Proto::Udp);
    assert_eq!(e.listener, "l");
    assert_eq!(e.pool.as_deref(), Some("p"));
    assert_eq!(e.backend, Some(backend));
    assert_eq!(e.peer, client.local_addr().unwrap());

    runtime
        .shutdown_with_grace(std::time::Duration::from_millis(100))
        .await;
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
    health_check: {{ type: udp_probe, send_hex: "00" }}
listeners:
  - name: l
    bind: "{proxy_addr}"
    protocol: udp
    pool: p
"#
    );
    let cfg = parse_str(&yaml).unwrap();
    let runtime = Runtime::start(Snapshot::from_config(&cfg), std::sync::Arc::default(), 1);
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

    runtime
        .shutdown_with_grace(std::time::Duration::from_millis(100))
        .await;
}

#[tokio::test]
async fn an_active_session_survives_past_its_idle_window_then_expires() {
    let backend = echo_backend(7).await;
    let proxy_addr = free_udp_addr();
    // idle_timeout 1s; one slot — a second client only gets in once the first
    // session is evicted, so "B still refused" proves A is still alive. The
    // echo backend is UDP-only, so probe it over UDP: the default `tcp_connect`
    // check fails against it and, with `fall: 3` at 2 s, marks it unhealthy about
    // 4 s in. That races the eviction this test waits for: B then gets
    // "no healthy backend" instead of the freed slot.
    let yaml = format!(
        r#"
pools:
  - name: p
    targets: ["{backend}"]
    idle_timeout_sec: 1
    per_backend: {{ max_sessions: 1 }}
    health_check: {{ type: udp_probe, send_hex: "00" }}
listeners:
  - name: l
    bind: "{proxy_addr}"
    protocol: udp
    pool: p
"#
    );
    let cfg = parse_str(&yaml).unwrap();
    let runtime = Runtime::start(Snapshot::from_config(&cfg), std::sync::Arc::default(), 1);
    tokio::time::sleep(Duration::from_millis(150)).await;

    let a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    a.connect(proxy_addr).await.unwrap();
    let mut buf = [0u8; 32];

    // The listener may not be up yet; get A's session established first. The
    // idle clock starts at the first datagram that reaches the proxy.
    let n = first_reply(&a, b"a", &mut buf).await;
    assert_eq!(&buf[..n], &[7, b'a']);

    // Keep A busy for ~2.5s — well past the 1s idle window. The timing wheel
    // must re-file it on every tick instead of evicting it.
    for _ in 0..8 {
        a.send(b"a").await.unwrap();
        let n = tokio::time::timeout(Duration::from_millis(500), a.recv(&mut buf))
            .await
            .expect("active session must keep round-tripping")
            .unwrap();
        assert_eq!(&buf[..n], &[7, b'a']);
        tokio::time::sleep(Duration::from_millis(320)).await;
    }

    // Still holding the only slot: a new client is refused.
    let b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    b.connect(proxy_addr).await.unwrap();
    b.send(b"b").await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(400), b.recv(&mut buf))
            .await
            .is_err(),
        "the still-active session A must not have been idle-evicted"
    );

    // Stop sending; within a couple of ticks A is evicted and B gets through.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    b.send(b"b").await.unwrap();
    let n = tokio::time::timeout(Duration::from_millis(500), b.recv(&mut buf))
        .await
        .expect("slot should free once A goes idle")
        .unwrap();
    assert_eq!(&buf[..n], &[7, b'b']);

    runtime
        .shutdown_with_grace(std::time::Duration::from_millis(100))
        .await;
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
    health_check:
      type: none
  - name: game
    targets: ["{game}"]
    health_check:
      type: none
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
    let runtime = Runtime::start(Snapshot::from_config(&cfg), std::sync::Arc::default(), 1);
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

    runtime
        .shutdown_with_grace(std::time::Duration::from_millis(100))
        .await;
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
    health_check:
      type: none
  - name: long
    targets: ["{long}"]
    health_check:
      type: none
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
    let runtime = Runtime::start(Snapshot::from_config(&cfg), std::sync::Arc::default(), 1);
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

    runtime
        .shutdown_with_grace(std::time::Duration::from_millis(100))
        .await;
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
    health_check:
      type: none
  - name: b
    targets: ["{b}"]
    health_check:
      type: none
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
    let runtime = Runtime::start(Snapshot::from_config(&cfg), std::sync::Arc::default(), 1);
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

    runtime
        .shutdown_with_grace(std::time::Duration::from_millis(100))
        .await;
}

#[tokio::test]
async fn shutdown_drains_udp_sessions_then_returns() {
    let b = echo_backend(9).await;
    let proxy_addr = free_udp_addr();

    // Short idle timeout so the drained session evicts quickly.
    let yaml = format!(
        r#"
pools:
  - name: p
    targets: ["{b}"]
    health_check:
      type: none
    idle_timeout_sec: 1
listeners:
  - name: l
    bind: "{proxy_addr}"
    protocol: udp
    pool: p
"#
    );
    let cfg = parse_str(&yaml).unwrap();
    let runtime = Runtime::start(Snapshot::from_config(&cfg), std::sync::Arc::default(), 1);
    tokio::time::sleep(Duration::from_millis(150)).await;

    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client.connect(proxy_addr).await.unwrap();
    client.send(b"one").await.unwrap();
    let mut buf = [0u8; 32];
    let n = tokio::time::timeout(Duration::from_millis(500), client.recv(&mut buf))
        .await
        .expect("first reply timed out")
        .unwrap();
    assert_eq!(&buf[1..n], b"one");

    // Begin shutdown with a generous grace while the session is live.
    let t0 = std::time::Instant::now();
    let sd = tokio::spawn(async move {
        runtime.shutdown_with_grace(Duration::from_secs(5)).await;
    });

    // The established session still forwards during the drain.
    client.send(b"two").await.unwrap();
    let n = tokio::time::timeout(Duration::from_millis(500), client.recv(&mut buf))
        .await
        .expect("in-flight udp session should keep working during drain")
        .unwrap();
    assert_eq!(&buf[1..n], b"two");

    // Stop sending: the session idle-evicts and the listener returns before the
    // 5 s grace deadline.
    sd.await.unwrap();
    assert!(
        t0.elapsed() < Duration::from_secs(5),
        "drain should finish when the session idles out, not at the grace deadline"
    );
}

#[tokio::test]
async fn prepends_a_v2_udp_proxy_header_to_the_first_datagram_only() {
    // Backend: capture the first datagram raw, echo the payload of every one.
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let backend = sock.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<Vec<u8>>();
    tokio::spawn(async move {
        let mut tx = Some(tx);
        let mut buf = [0u8; 2048];
        while let Ok((n, peer)) = sock.recv_from(&mut buf).await {
            if let Some(tx) = tx.take() {
                let _ = tx.send(buf[..n].to_vec());
                // First datagram carries the 28-byte v2 header; echo the rest.
                let _ = sock.send_to(&buf[28..n], peer).await;
            } else {
                let _ = sock.send_to(&buf[..n], peer).await;
            }
        }
    });

    let proxy_addr = free_udp_addr();
    let yaml = format!(
        r#"
pools:
  - name: p
    targets: ["{backend}"]
    health_check:
      type: none
    proxy_protocol: v2-udp
listeners:
  - name: l
    bind: "{proxy_addr}"
    protocol: udp
    pool: p
"#
    );
    let cfg = parse_str(&yaml).unwrap();
    let runtime = Runtime::start(Snapshot::from_config(&cfg), std::sync::Arc::default(), 1);
    tokio::time::sleep(Duration::from_millis(150)).await;

    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client.connect(proxy_addr).await.unwrap();
    let client_addr = client.local_addr().unwrap();

    client.send(b"first").await.unwrap();
    let mut buf = [0u8; 64];
    let n = tokio::time::timeout(Duration::from_millis(500), client.recv(&mut buf))
        .await
        .expect("reply timed out")
        .unwrap();
    assert_eq!(&buf[..n], b"first");

    let hdr = tokio::time::timeout(Duration::from_secs(1), rx)
        .await
        .unwrap()
        .unwrap();
    // v2 signature + PROXY/AF_INET/DGRAM + 12-byte addr block + "first".
    assert_eq!(
        &hdr[..12],
        &[0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A]
    );
    assert_eq!(hdr[12], 0x21);
    assert_eq!(hdr[13], 0x12); // AF_INET + DGRAM
    assert_eq!(&hdr[24..26], &client_addr.port().to_be_bytes());
    assert_eq!(&hdr[26..28], &proxy_addr.port().to_be_bytes());
    assert_eq!(&hdr[28..], b"first");

    // Second datagram must NOT carry a header (backend echoes it verbatim).
    client.send(b"second").await.unwrap();
    let n = tokio::time::timeout(Duration::from_millis(500), client.recv(&mut buf))
        .await
        .expect("reply timed out")
        .unwrap();
    assert_eq!(&buf[..n], b"second");

    runtime
        .shutdown_with_grace(std::time::Duration::from_millis(100))
        .await;
}

#[tokio::test]
async fn first_packet_gate_drops_unrecognised_datagrams() {
    let b = echo_backend(7).await;
    let proxy_addr = free_udp_addr();

    // Gate on: only datagrams starting 0xFFFFFFFF create a session.
    let yaml = format!(
        r#"
pools:
  - name: p
    targets: ["{b}"]
    health_check:
      type: none
listeners:
  - name: l
    bind: "{proxy_addr}"
    protocol: udp
    first_packet_gate: true
    routes:
      - match: {{ type: first_bytes, prefix: "hex:ffffffff" }}
        action: {{ pool: p }}
      - match: {{ type: always }}
        action: {{ pool: p }}
"#
    );
    let cfg = parse_str(&yaml).unwrap();
    let runtime = Runtime::start(Snapshot::from_config(&cfg), std::sync::Arc::default(), 1);
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Unrecognised first datagram: no session, no reply.
    let bad = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    bad.connect(proxy_addr).await.unwrap();
    bad.send(b"not a handshake").await.unwrap();
    let mut buf = [0u8; 64];
    let r = tokio::time::timeout(Duration::from_millis(300), bad.recv(&mut buf)).await;
    assert!(r.is_err(), "gated datagram must get no reply, got {r:?}");

    // Recognised first datagram: session opens, echo comes back.
    let good = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    good.connect(proxy_addr).await.unwrap();
    good.send(&[0xff, 0xff, 0xff, 0xff, 0x10]).await.unwrap();
    let n = tokio::time::timeout(Duration::from_millis(500), good.recv(&mut buf))
        .await
        .expect("recognised datagram should be forwarded")
        .unwrap();
    assert_eq!(buf[0], 7, "reply should come from the echo backend");
    assert_eq!(&buf[1..n], &[0xff, 0xff, 0xff, 0xff, 0x10]);

    runtime
        .shutdown_with_grace(std::time::Duration::from_millis(100))
        .await;
}

#[tokio::test]
async fn icmp_port_unreachable_marks_the_backend_unhealthy() {
    // The backend port has a TCP listener (so the default `tcp_connect` active
    // check keeps passing) but nothing on UDP: the connected upstream socket
    // draws an ICMP port-unreachable, surfaced as `ConnectionRefused` and fed
    // into passive health. Only the passive UDP path can flip this backend.
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead = tcp.local_addr().unwrap();
    tokio::spawn(async move { while tcp.accept().await.is_ok() {} });
    let proxy_addr = free_udp_addr();

    let yaml = format!(
        r#"
pools:
  - name: p
    targets: ["{dead}"]
    health_check: {{ type: tcp_connect, interval_sec: 1, timeout_ms: 200, rise: 1, fall: 1 }}
listeners:
  - name: l
    bind: "{proxy_addr}"
    protocol: udp
    pool: p
"#
    );
    let cfg = parse_str(&yaml).unwrap();
    let runtime = Runtime::start(Snapshot::from_config(&cfg), std::sync::Arc::default(), 1);
    tokio::time::sleep(Duration::from_millis(150)).await;

    let backend = runtime
        .handle()
        .snapshot()
        .pool("p")
        .unwrap()
        .backend(dead)
        .unwrap()
        .clone();
    assert!(
        backend.is_healthy(),
        "backend starts optimistically healthy"
    );

    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client.connect(proxy_addr).await.unwrap();
    client.send(b"hello").await.unwrap();

    let mut flipped = false;
    for _ in 0..40 {
        if !backend.is_healthy() {
            flipped = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        flipped,
        "port-unreachable should mark the backend unhealthy"
    );

    runtime
        .shutdown_with_grace(std::time::Duration::from_millis(100))
        .await;
}

/// An echo backend with a full-size receive buffer, so large datagrams make
/// the round trip.
async fn big_echo_backend() -> std::net::SocketAddr {
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = sock.local_addr().unwrap();
    tokio::spawn(async move {
        let mut buf = vec![0u8; 65_536];
        while let Ok((n, peer)) = sock.recv_from(&mut buf).await {
            let _ = sock.send_to(&buf[..n], peer).await;
        }
    });
    addr
}

#[tokio::test]
async fn replies_of_every_size_reach_their_own_client_intact() {
    let backend = big_echo_backend().await;
    let proxy_addr = free_udp_addr();
    let yaml = format!(
        r#"
pools:
  - name: p
    targets: ["{backend}"]
    health_check:
      type: none
listeners:
  - name: l
    bind: "{proxy_addr}"
    protocol: udp
    pool: p
"#
    );
    let cfg = parse_str(&yaml).unwrap();
    let runtime = Runtime::start(Snapshot::from_config(&cfg), std::sync::Arc::default(), 1);
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Concurrent sessions, each with its own fill byte and datagram size, from
    // a tiny one up to near the UDP maximum. The reply pumps interleave on the
    // runtime, so any shared reply buffer must not mix their payloads up.
    let sizes = [1usize, 64, 1_200, 9_000, 30_000, 60_000, 65_000, 100];
    let mut tasks = Vec::new();
    for (i, size) in sizes.into_iter().enumerate() {
        tasks.push(tokio::spawn(async move {
            let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            client.connect(proxy_addr).await.unwrap();
            let payload = vec![i as u8 + 1; size];
            let mut buf = vec![0u8; 65_536];
            let n = first_reply(&client, &payload, &mut buf).await;
            assert_eq!(&buf[..n], &payload[..], "session {i} got a corrupted reply");
            for round in 0..20 {
                client.send(&payload).await.unwrap();
                let n = tokio::time::timeout(Duration::from_secs(2), client.recv(&mut buf))
                    .await
                    .expect("reply timed out")
                    .unwrap();
                assert_eq!(&buf[..n], &payload[..], "session {i} round {round}");
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    runtime
        .shutdown_with_grace(std::time::Duration::from_millis(100))
        .await;
}

#[tokio::test]
async fn consistent_hash_keeps_a_client_ip_on_one_backend_across_ports() {
    let b1 = echo_backend(1).await;
    let b2 = echo_backend(2).await;
    let proxy_addr = free_udp_addr();
    let yaml = format!(
        r#"
pools:
  - name: p
    targets: ["{b1}", "{b2}"]
    health_check:
      type: none
    balancer: consistent_hash
    hash_on: src_ip
listeners:
  - name: l
    bind: "{proxy_addr}"
    protocol: udp
    pool: p
"#
    );
    let cfg = parse_str(&yaml).unwrap();
    let runtime = Runtime::start(Snapshot::from_config(&cfg), std::sync::Arc::default(), 2);
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Each socket is a new source port (a new session, possibly on another
    // worker); `src_ip` hashing must still send them all to the same backend.
    let mut tags = Vec::new();
    for _ in 0..12 {
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client.connect(proxy_addr).await.unwrap();
        let mut buf = [0u8; 32];
        let n = first_reply(&client, b"hi", &mut buf).await;
        assert_eq!(&buf[1..n], b"hi");
        tags.push(buf[0]);
    }
    assert!(
        tags.windows(2).all(|w| w[0] == w[1]),
        "one client IP must hash to one backend, got {tags:?}"
    );
    runtime
        .shutdown_with_grace(std::time::Duration::from_millis(100))
        .await;
}

/// A UDP socket that swallows every datagram: the connected send succeeds
/// locally, but the backend never answers.
async fn silent_backend() -> std::net::SocketAddr {
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = sock.local_addr().unwrap();
    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        while sock.recv_from(&mut buf).await.is_ok() {}
    });
    addr
}

#[tokio::test]
async fn a_stream_of_new_sessions_does_not_hide_a_failing_active_check() {
    // The backend's port has no TCP listener, so the explicit `tcp_connect`
    // probe fails every second; the backend takes datagrams but never answers.
    // A passive "success" per new session used to wipe the failure streak.
    let backend = silent_backend().await;
    let proxy_addr = free_udp_addr();
    let yaml = format!(
        r#"
pools:
  - name: p
    targets: ["{backend}"]
    health_check: {{ type: tcp_connect, interval_sec: 1, timeout_ms: 200, rise: 2, fall: 2 }}
listeners:
  - name: l
    bind: "{proxy_addr}"
    protocol: udp
    pool: p
"#
    );
    let cfg = parse_str(&yaml).unwrap();
    let runtime = Runtime::start(Snapshot::from_config(&cfg), std::sync::Arc::default(), 1);
    tokio::time::sleep(Duration::from_millis(150)).await;
    let be = runtime
        .handle()
        .snapshot()
        .pool("p")
        .unwrap()
        .backend(backend)
        .unwrap()
        .clone();

    let mut flipped = false;
    for _ in 0..120 {
        // A fresh client (new session) every 50 ms.
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let _ = client.send_to(b"hi", proxy_addr).await;
        if !be.is_healthy() {
            flipped = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        flipped,
        "failing probes must take the backend down despite traffic"
    );
    runtime
        .shutdown_with_grace(std::time::Duration::from_millis(100))
        .await;
}

#[tokio::test]
async fn health_check_none_keeps_a_udp_only_backend_in_rotation() {
    // The #124 setup: a UDP-only backend and no probe payload. It must stay
    // healthy well past `fall` x `interval`, and keep answering new sessions.
    let backend = echo_backend(3).await;
    let proxy_addr = free_udp_addr();
    let yaml = format!(
        r#"
pools:
  - name: p
    targets: ["{backend}"]
    health_check: {{ type: none, interval_sec: 1, fall: 1 }}
listeners:
  - name: l
    bind: "{proxy_addr}"
    protocol: udp
    pool: p
"#
    );
    let cfg = parse_str(&yaml).unwrap();
    let runtime = Runtime::start(Snapshot::from_config(&cfg), std::sync::Arc::default(), 1);
    tokio::time::sleep(Duration::from_millis(150)).await;
    tokio::time::sleep(Duration::from_millis(2500)).await;

    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client.connect(proxy_addr).await.unwrap();
    let mut buf = [0u8; 64];
    let n = first_reply(&client, b"x", &mut buf).await;
    assert_eq!(&buf[..n], &[3, b'x']);
    runtime
        .shutdown_with_grace(std::time::Duration::from_millis(100))
        .await;
}

/// #150: a second instance with the same UDP bind must fail to start rather
/// than share the port through `SO_REUSEPORT`.
#[tokio::test]
async fn a_second_udp_instance_with_the_same_bind_fails_to_start() {
    let b1 = echo_backend(1).await;
    let proxy_addr = free_udp_addr();
    let yaml = format!(
        "pools:\n  - name: p\n    targets: [\"{b1}\"]\n    health_check:\n      type: none\n\
         listeners:\n  - name: l\n    bind: \"{proxy_addr}\"\n    protocol: udp\n    pool: p\n"
    );
    let snap = || Snapshot::from_config(&parse_str(&yaml).unwrap());
    let first = Runtime::start(snap(), std::sync::Arc::default(), 2);

    let second = Runtime::start_with_discovery(
        snap(),
        std::sync::Arc::default(),
        None,
        std::sync::Arc::default(),
        std::sync::Arc::new(wayhouse_core::Discovery::new()),
        None,
        None,
        2,
    );
    let err = second.err().expect("a duplicate bind must fail startup");
    assert_eq!(err.bind, proxy_addr);
    assert_eq!(err.source.kind(), std::io::ErrorKind::AddrInUse);

    first.shutdown_with_grace(Duration::from_millis(100)).await;
}

/// Replacing a UDP listener must not hold the reload until its live sessions
/// idle out: a client that keeps sending would hold the drain open forever.
#[tokio::test]
async fn changing_a_udp_listener_does_not_block_reconcile_on_live_sessions() {
    let backend = echo_backend(1).await;
    let proxy_addr = free_udp_addr();
    let yaml = |extra: &str| {
        format!(
            r#"
pools:
  - name: p
    targets: ["{backend}"]
    health_check:
      type: none
listeners:
  - name: l
    bind: "{proxy_addr}"
    protocol: udp
    pool: p
{extra}"#
        )
    };
    let cfg1 = parse_str(&yaml("")).unwrap();
    let runtime = Runtime::start(Snapshot::from_config(&cfg1), std::sync::Arc::default(), 1);
    let handle = runtime.handle();

    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client.connect(proxy_addr).await.unwrap();
    let mut buf = [0u8; 64];
    first_reply(&client, b"hb", &mut buf).await;
    // Keep the session alive for the whole test.
    let client = std::sync::Arc::new(client);
    let pinger = {
        let client = client.clone();
        tokio::spawn(async move {
            loop {
                let _ = client.send(b"hb").await;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
    };

    let cfg2 = parse_str(&yaml(
        "    rate_limit:\n      per_ip: { rate: 1000, burst: 1000 }\n",
    ))
    .unwrap();
    handle.store(Snapshot::build_with_overlay(
        &cfg2,
        Some(&handle.current()),
        handle.backend_overlay(),
    ));
    let r = tokio::time::timeout(Duration::from_secs(5), handle.reconcile_listeners()).await;
    pinger.abort();
    let out = r.expect("reconcile must return while a UDP session is live");
    assert_eq!(out.stopped, 1);
    assert!(out.failed.is_empty());
}
