//! End-to-end smoke test: bytes flow client -> proxy -> backend -> client.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use gsp_config::parse_str;
use gsp_core::{Runtime, Snapshot};

#[tokio::test]
async fn forwards_tcp_bytes_end_to_end() {
    // Backend: a trivial echo server on an ephemeral port.
    let backend = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_addr = backend.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = backend.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                loop {
                    match s.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if s.write_all(&buf[..n]).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }
    });

    // Grab a free port for the proxy listener, then release it.
    let proxy_addr = {
        let probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
        probe.local_addr().unwrap()
    };

    let yaml = format!(
        "pools:\n  - name: p\n    targets: [\"{backend_addr}\"]\n\
         listeners:\n  - name: l\n    bind: \"{proxy_addr}\"\n    pool: p\n"
    );
    let cfg = parse_str(&yaml).unwrap();
    let snapshot: Arc<Snapshot> = Snapshot::from_config(&cfg);
    let runtime = Runtime::start(snapshot, Default::default(), 1);

    // Let the listener bind.
    tokio::time::sleep(Duration::from_millis(150)).await;

    let mut client = TcpStream::connect(proxy_addr).await.unwrap();
    client.write_all(b"hello proxy").await.unwrap();
    let mut buf = [0u8; 11];
    client.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"hello proxy");

    drop(client);
    runtime
        .shutdown_with_grace(std::time::Duration::from_millis(100))
        .await;
}

#[tokio::test]
async fn splice_forwards_a_large_stream_and_propagates_half_close() {
    // Echo backend that closes its side once the client half-closes.
    let backend = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_addr = backend.local_addr().unwrap();
    tokio::spawn(async move {
        // Loop so the active health-check probe and the real connection are both
        // served.
        while let Ok((mut s, _)) = backend.accept().await {
            tokio::spawn(async move {
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    match s.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if s.write_all(&buf[..n]).await.is_err() {
                                break;
                            }
                        }
                    }
                }
                // Client EOF seen: drop `s` so the client's read side sees EOF.
            });
        }
    });

    let proxy_addr = {
        let probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
        probe.local_addr().unwrap()
    };
    let yaml = format!(
        "pools:\n  - name: p\n    targets: [\"{backend_addr}\"]\n\
         listeners:\n  - name: l\n    bind: \"{proxy_addr}\"\n    pool: p\n"
    );
    let cfg = parse_str(&yaml).unwrap();
    let runtime = Runtime::start(Snapshot::from_config(&cfg), Default::default(), 1);
    tokio::time::sleep(Duration::from_millis(150)).await;

    let stream = TcpStream::connect(proxy_addr).await.unwrap();
    let (mut rd, mut wr) = stream.into_split();

    const TOTAL: usize = 4 * 1024 * 1024;
    let writer = tokio::spawn(async move {
        let chunk = vec![0xABu8; 64 * 1024];
        let mut sent = 0;
        while sent < TOTAL {
            let n = (TOTAL - sent).min(chunk.len());
            wr.write_all(&chunk[..n]).await.unwrap();
            sent += n;
        }
        wr.shutdown().await.unwrap(); // half-close: backend must see EOF
    });

    let mut got = 0usize;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = rd.read(&mut buf).await.unwrap();
        if n == 0 {
            break; // backend closed after seeing our half-close, proxied back
        }
        assert!(buf[..n].iter().all(|&b| b == 0xAB), "payload corrupted");
        got += n;
    }
    writer.await.unwrap();
    assert_eq!(got, TOTAL, "every byte echoed back through the splice path");

    runtime
        .shutdown_with_grace(std::time::Duration::from_millis(100))
        .await;
}

#[tokio::test]
async fn routes_around_a_dead_backend() {
    // One live echo backend.
    let alive = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let alive_addr = alive.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = alive.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                while let Ok(n) = s.read(&mut buf).await {
                    if n == 0 || s.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });

    // A dead address: bind then release so nothing is listening there.
    let dead_addr = {
        let p = TcpListener::bind("127.0.0.1:0").await.unwrap();
        p.local_addr().unwrap()
    };
    let proxy_addr = {
        let p = TcpListener::bind("127.0.0.1:0").await.unwrap();
        p.local_addr().unwrap()
    };

    // Dead backend is first in round-robin. fall=1 so one failed connect (the
    // passive signal from the first request) marks it unhealthy immediately.
    let yaml = format!(
        r#"
pools:
  - name: p
    targets: ["{dead_addr}", "{alive_addr}"]
    balancer: round_robin
    health_check: {{ interval_sec: 1, timeout_ms: 200, rise: 1, fall: 1 }}
listeners:
  - name: l
    bind: "{proxy_addr}"
    pool: p
"#
    );
    let cfg = parse_str(&yaml).unwrap();
    let runtime = Runtime::start(Snapshot::from_config(&cfg), Default::default(), 1);
    tokio::time::sleep(Duration::from_millis(150)).await;

    let mut successes = 0;
    for _ in 0..10 {
        if let Ok(mut c) = TcpStream::connect(proxy_addr).await {
            if c.write_all(b"x").await.is_ok() {
                let mut b = [0u8; 1];
                if tokio::time::timeout(Duration::from_millis(300), c.read_exact(&mut b))
                    .await
                    .is_ok_and(|r| r.is_ok())
                {
                    successes += 1;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    assert!(
        successes >= 8,
        "expected most requests to succeed, got {successes}/10"
    );
    runtime
        .shutdown_with_grace(std::time::Duration::from_millis(100))
        .await;
}

/// Spawn a backend that, on each connection, writes a single identifying byte.
async fn marker_backend(mark: u8) -> std::net::SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            tokio::spawn(async move {
                let _ = s.write_all(&[mark]).await;
                let mut buf = [0u8; 64];
                while let Ok(n) = s.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                }
            });
        }
    });
    addr
}

async fn free_port() -> std::net::SocketAddr {
    TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
}

/// Two adjacent free ports on 127.0.0.1, for the bind-range test — retries a
/// few times since "port N+1 is also free right now" isn't guaranteed by a
/// single ephemeral-port grab.
async fn free_port_pair() -> (u16, u16) {
    for _ in 0..20 {
        let lo = free_port().await.port();
        if lo == u16::MAX {
            continue;
        }
        if TcpListener::bind(("127.0.0.1", lo + 1)).await.is_ok() {
            return (lo, lo + 1);
        }
    }
    panic!("couldn't find two adjacent free ports after 20 tries");
}

#[tokio::test]
async fn first_matching_route_selects_the_pool() {
    let a = marker_backend(b'A').await;
    let b = marker_backend(b'B').await;

    // Case 1: the client's /32 matches route 1 -> pool a.
    let p1 = free_port().await;
    // Case 2: route 1's CIDR excludes the loopback client -> falls through to b.
    let p2 = free_port().await;

    let yaml = format!(
        r#"
pools:
  - name: a
    targets: ["{a}"]
  - name: b
    targets: ["{b}"]
listeners:
  - name: hit
    bind: "{p1}"
    routes:
      - match: {{ type: client_cidr, cidrs: ["127.0.0.1/32"] }}
        action: {{ pool: a }}
      - match: {{ type: always }}
        action: {{ pool: b }}
  - name: miss
    bind: "{p2}"
    routes:
      - match: {{ type: client_cidr, cidrs: ["10.0.0.0/8"] }}
        action: {{ pool: a }}
      - match: {{ type: always }}
        action: {{ pool: b }}
"#
    );
    let cfg = parse_str(&yaml).unwrap();
    let runtime = Runtime::start(Snapshot::from_config(&cfg), Default::default(), 1);
    tokio::time::sleep(Duration::from_millis(150)).await;

    let mut c1 = TcpStream::connect(p1).await.unwrap();
    let mut m1 = [0u8; 1];
    c1.read_exact(&mut m1).await.unwrap();
    assert_eq!(m1[0], b'A', "loopback /32 route should reach pool a");

    let mut c2 = TcpStream::connect(p2).await.unwrap();
    let mut m2 = [0u8; 1];
    c2.read_exact(&mut m2).await.unwrap();
    assert_eq!(
        m2[0], b'B',
        "non-matching CIDR should fall through to pool b"
    );

    runtime
        .shutdown_with_grace(std::time::Duration::from_millis(100))
        .await;
}

#[tokio::test]
async fn bind_port_range_listener_serves_every_port_and_routes_by_it() {
    // F1.4: one listener config, `bind: "host:lo-hi"`, spawns a real socket per
    // port; each port still routes independently via the `port` matcher.
    let a = marker_backend(b'A').await;
    let b = marker_backend(b'B').await;
    let (lo, hi) = free_port_pair().await;

    let yaml = format!(
        r#"
pools:
  - name: a
    targets: ["{a}"]
  - name: b
    targets: ["{b}"]
listeners:
  - name: ranged
    bind: "127.0.0.1:{lo}-{hi}"
    routes:
      - match: {{ type: port, ports: [{lo}] }}
        action: {{ pool: a }}
      - match: {{ type: always }}
        action: {{ pool: b }}
"#
    );
    let cfg = parse_str(&yaml).unwrap();
    assert_eq!(cfg.listeners[0].extra_binds.len(), 1);
    let runtime = Runtime::start(Snapshot::from_config(&cfg), Default::default(), 1);
    tokio::time::sleep(Duration::from_millis(150)).await;

    let mut c1 = TcpStream::connect(("127.0.0.1", lo)).await.unwrap();
    let mut m1 = [0u8; 1];
    c1.read_exact(&mut m1).await.unwrap();
    assert_eq!(m1[0], b'A', "the low port of the range should reach pool a");

    let mut c2 = TcpStream::connect(("127.0.0.1", hi)).await.unwrap();
    let mut m2 = [0u8; 1];
    c2.read_exact(&mut m2).await.unwrap();
    assert_eq!(
        m2[0], b'B',
        "the high port of the range should fall through to pool b"
    );

    runtime
        .shutdown_with_grace(std::time::Duration::from_millis(100))
        .await;
}

#[tokio::test]
async fn draining_a_backend_diverts_new_connections() {
    let a = marker_backend(b'A').await;
    let b = marker_backend(b'B').await;
    let proxy = free_port().await;

    let yaml = format!(
        r#"
pools:
  - name: p
    targets: ["{a}", "{b}"]
    balancer: round_robin
listeners:
  - name: l
    bind: "{proxy}"
    pool: p
"#
    );
    let cfg = parse_str(&yaml).unwrap();
    let runtime = Runtime::start(Snapshot::from_config(&cfg), Default::default(), 1);
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Drain backend A: every new connection must now land on B.
    runtime
        .handle()
        .snapshot()
        .pool("p")
        .unwrap()
        .backend(a)
        .unwrap()
        .set_admin_state(gsp_core::pool::AdminState::Draining);

    for _ in 0..8 {
        let mut c = TcpStream::connect(proxy).await.unwrap();
        let mut m = [0u8; 1];
        c.read_exact(&mut m).await.unwrap();
        assert_eq!(m[0], b'B', "drained backend A must get no new connections");
    }

    runtime
        .shutdown_with_grace(std::time::Duration::from_millis(100))
        .await;
}

#[tokio::test]
async fn consistent_hash_pins_a_client_to_one_backend() {
    let a = marker_backend(b'A').await;
    let b = marker_backend(b'B').await;
    let c = marker_backend(b'C').await;
    let proxy_addr = free_port().await;

    let yaml = format!(
        r#"
pools:
  - name: p
    targets: ["{a}", "{b}", "{c}"]
    balancer: consistent_hash
    hash_on: src_ip
listeners:
  - name: l
    bind: "{proxy_addr}"
    pool: p
"#
    );
    let cfg = parse_str(&yaml).unwrap();
    let runtime = Runtime::start(Snapshot::from_config(&cfg), Default::default(), 1);
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Same source IP (127.0.0.1), different ephemeral ports each connection.
    let mut seen = std::collections::BTreeSet::new();
    for _ in 0..12 {
        let mut conn = TcpStream::connect(proxy_addr).await.unwrap();
        let mut m = [0u8; 1];
        conn.read_exact(&mut m).await.unwrap();
        seen.insert(m[0]);
    }
    assert_eq!(
        seen.len(),
        1,
        "src_ip hashing must pin one client IP to a single backend, saw {seen:?}"
    );

    runtime
        .shutdown_with_grace(std::time::Duration::from_millis(100))
        .await;
}

/// A minimal TLS ClientHello record carrying `sni`.
fn client_hello(sni: &str) -> Vec<u8> {
    let mut sn = Vec::new();
    sn.extend_from_slice(&((sni.len() + 3) as u16).to_be_bytes());
    sn.push(0x00);
    sn.extend_from_slice(&(sni.len() as u16).to_be_bytes());
    sn.extend_from_slice(sni.as_bytes());

    let mut ext = Vec::new();
    ext.extend_from_slice(&0u16.to_be_bytes());
    ext.extend_from_slice(&(sn.len() as u16).to_be_bytes());
    ext.extend_from_slice(&sn);

    let mut body = Vec::new();
    body.extend_from_slice(&[0x03, 0x03]);
    body.extend_from_slice(&[0u8; 32]);
    body.push(0x00);
    body.extend_from_slice(&2u16.to_be_bytes());
    body.extend_from_slice(&[0x00, 0x2f]);
    body.push(0x01);
    body.push(0x00);
    body.extend_from_slice(&(ext.len() as u16).to_be_bytes());
    body.extend_from_slice(&ext);

    let bl = body.len();
    let mut hs = vec![0x01, (bl >> 16) as u8, (bl >> 8) as u8, bl as u8];
    hs.extend_from_slice(&body);

    let mut rec = vec![0x16, 0x03, 0x01];
    rec.extend_from_slice(&(hs.len() as u16).to_be_bytes());
    rec.extend_from_slice(&hs);
    rec
}

#[tokio::test]
async fn sni_matcher_routes_by_client_hello() {
    let eu = marker_backend(b'E').await;
    let lobby = marker_backend(b'L').await;
    let proxy_addr = free_port().await;

    let yaml = format!(
        r#"
pools:
  - name: eu
    targets: ["{eu}"]
  - name: lobby
    targets: ["{lobby}"]
listeners:
  - name: l
    bind: "{proxy_addr}"
    routes:
      - match: {{ type: sni, host: ["*.eu.example.com"] }}
        action: {{ pool: eu }}
      - match: {{ type: always }}
        action: {{ pool: lobby }}
"#
    );
    let cfg = parse_str(&yaml).unwrap();
    let runtime = Runtime::start(Snapshot::from_config(&cfg), Default::default(), 1);
    tokio::time::sleep(Duration::from_millis(150)).await;

    let hit_mark = |host: &'static str| async move {
        let mut c = TcpStream::connect(proxy_addr).await.unwrap();
        c.write_all(&client_hello(host)).await.unwrap();
        let mut m = [0u8; 1];
        c.read_exact(&mut m).await.unwrap();
        m[0]
    };

    assert_eq!(hit_mark("frankfurt.eu.example.com").await, b'E');
    assert_eq!(hit_mark("us.example.com").await, b'L');

    runtime
        .shutdown_with_grace(std::time::Duration::from_millis(100))
        .await;
}

/// A ClientHello that arrives split across two TCP segments (a large real-world
/// ClientHello routinely exceeds one MSS) must still route by SNI — the listener
/// re-peeks until the whole first TLS record is buffered.
#[tokio::test]
async fn sni_matcher_reassembles_a_fragmented_client_hello() {
    let eu = marker_backend(b'E').await;
    let lobby = marker_backend(b'L').await;
    let proxy_addr = free_port().await;

    let yaml = format!(
        r#"
pools:
  - name: eu
    targets: ["{eu}"]
  - name: lobby
    targets: ["{lobby}"]
listeners:
  - name: l
    bind: "{proxy_addr}"
    routes:
      - match: {{ type: sni, host: ["*.eu.example.com"] }}
        action: {{ pool: eu }}
      - match: {{ type: always }}
        action: {{ pool: lobby }}
"#
    );
    let cfg = parse_str(&yaml).unwrap();
    let runtime = Runtime::start(Snapshot::from_config(&cfg), Default::default(), 1);
    tokio::time::sleep(Duration::from_millis(150)).await;

    let hello = client_hello("frankfurt.eu.example.com");
    // Split mid-record so a single peek can only ever see the first half.
    let split = hello.len() / 2;

    let mut c = TcpStream::connect(proxy_addr).await.unwrap();
    c.write_all(&hello[..split]).await.unwrap();
    c.flush().await.unwrap();
    // Longer than one PEEK_POLL, well under the 250 ms PEEK_TIMEOUT.
    tokio::time::sleep(Duration::from_millis(40)).await;
    c.write_all(&hello[split..]).await.unwrap();
    c.flush().await.unwrap();

    let mut m = [0u8; 1];
    c.read_exact(&mut m).await.unwrap();
    assert_eq!(
        m[0], b'E',
        "fragmented ClientHello should still route by SNI"
    );

    runtime
        .shutdown_with_grace(std::time::Duration::from_millis(100))
        .await;
}

#[tokio::test]
async fn route_hint_overrides_the_route_list_for_a_source_ip() {
    let hinted = marker_backend(b'H').await;
    let normal = marker_backend(b'N').await;
    let proxy_addr = free_port().await;

    let yaml = format!(
        r#"
pools:
  - name: hinted
    targets: ["{hinted}"]
  - name: normal
    targets: ["{normal}"]
listeners:
  - name: l
    bind: "{proxy_addr}"
    route_hint: true
    routes:
      - match: {{ type: always }}
        action: {{ pool: normal }}
"#
    );
    let cfg = parse_str(&yaml).unwrap();
    let runtime = Runtime::start(Snapshot::from_config(&cfg), Default::default(), 1);
    tokio::time::sleep(Duration::from_millis(150)).await;

    let hit = || async {
        let mut c = TcpStream::connect(proxy_addr).await.unwrap();
        let mut m = [0u8; 1];
        c.read_exact(&mut m).await.unwrap();
        m[0]
    };

    // No hint yet: the `always` route wins.
    assert_eq!(hit().await, b'N');

    // Push a hint for the loopback client; now it wins.
    runtime.handle().route_hints().set(
        "127.0.0.1".parse().unwrap(),
        "hinted".into(),
        Duration::from_secs(30),
    );
    assert_eq!(hit().await, b'H');

    // A hint naming a pool that does not exist is ignored.
    runtime.handle().route_hints().set(
        "127.0.0.1".parse().unwrap(),
        "ghost".into(),
        Duration::from_secs(30),
    );
    assert_eq!(hit().await, b'N');

    runtime
        .shutdown_with_grace(std::time::Duration::from_millis(100))
        .await;
}

#[tokio::test]
async fn shutdown_drains_in_flight_connections_then_returns_early() {
    // Backend echoes each write back after a short delay, keeping the
    // connection busy across the shutdown signal.
    let backend = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let baddr = backend.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = backend.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 64];
                while let Ok(n) = s.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    if s.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    let proxy = free_port().await;
    let yaml = format!(
        "pools:\n  - name: p\n    targets: [\"{baddr}\"]\n\
         listeners:\n  - name: l\n    bind: \"{proxy}\"\n    pool: p\n"
    );
    let cfg = parse_str(&yaml).unwrap();
    let runtime = Runtime::start(Snapshot::from_config(&cfg), Default::default(), 1);
    tokio::time::sleep(Duration::from_millis(150)).await;

    let mut c = TcpStream::connect(proxy).await.unwrap();
    c.write_all(b"ping").await.unwrap();

    // Start a shutdown with a long grace while the request is still in flight.
    let t0 = std::time::Instant::now();
    let sd = tokio::spawn(async move {
        runtime.shutdown_with_grace(Duration::from_secs(5)).await;
    });

    // The connection accepted before shutdown still completes.
    let mut buf = [0u8; 4];
    tokio::time::timeout(Duration::from_secs(2), c.read_exact(&mut buf))
        .await
        .expect("in-flight response should not be cut off by shutdown")
        .unwrap();
    assert_eq!(&buf, b"ping");
    drop(c);

    sd.await.unwrap();
    assert!(
        t0.elapsed() < Duration::from_secs(5),
        "shutdown should return once the connection drains, not wait out the full grace"
    );

    // New connections are refused once the listener has stopped.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(TcpStream::connect(proxy).await.is_err());
}

#[tokio::test]
async fn sessions_registry_lists_a_live_connection_with_its_pool_and_backend() {
    // Slow-echo backend: keeps the connection open while we inspect the registry.
    let backend = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let baddr = backend.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = backend.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 64];
                while let Ok(n) = s.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    if s.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    let proxy = free_port().await;
    let yaml = format!(
        "pools:\n  - name: p\n    targets: [\"{baddr}\"]\n\
         listeners:\n  - name: l\n    bind: \"{proxy}\"\n    pool: p\n"
    );
    let cfg = parse_str(&yaml).unwrap();
    let runtime = Runtime::start(Snapshot::from_config(&cfg), Default::default(), 1);
    let handle = runtime.handle();
    tokio::time::sleep(Duration::from_millis(150)).await;

    assert!(handle.sessions().is_empty());

    let mut c = TcpStream::connect(proxy).await.unwrap();
    c.write_all(b"ping").await.unwrap();
    // Let the accept task route and pick a backend.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let live = handle.sessions();
    assert_eq!(live.len(), 1, "one live session expected");
    let e = &live[0];
    assert_eq!(e.proto, gsp_core::Proto::Tcp);
    assert_eq!(e.listener, "l");
    assert_eq!(e.pool.as_deref(), Some("p"));
    assert_eq!(e.backend, Some(baddr));
    assert_eq!(e.peer, c.local_addr().unwrap());
    assert_eq!(handle.active_conns(), 1);

    drop(c);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        handle.sessions().is_empty(),
        "session drops out of the registry when the connection closes"
    );

    runtime
        .shutdown_with_grace(std::time::Duration::from_millis(100))
        .await;
}

#[tokio::test]
async fn admin_drain_flips_readiness_without_stopping_the_data_path() {
    let a = marker_backend(b'A').await;
    let proxy = free_port().await;
    let yaml = format!(
        "pools:\n  - name: p\n    targets: [\"{a}\"]\n\
         listeners:\n  - name: l\n    bind: \"{proxy}\"\n    pool: p\n"
    );
    let cfg = parse_str(&yaml).unwrap();
    let runtime = Runtime::start(Snapshot::from_config(&cfg), Default::default(), 1);
    let handle = runtime.handle();
    tokio::time::sleep(Duration::from_millis(150)).await;

    assert!(handle.ready());
    assert!(!handle.is_draining());

    handle.set_draining(true);
    assert!(!handle.ready(), "drain must make readyz fail");
    assert!(handle.is_draining());

    // The data path keeps working while drained.
    let mut c = TcpStream::connect(proxy).await.unwrap();
    let mut m = [0u8; 1];
    c.read_exact(&mut m).await.unwrap();
    assert_eq!(m[0], b'A');

    handle.set_draining(false);
    assert!(handle.ready(), "undrain restores readiness");

    runtime
        .shutdown_with_grace(std::time::Duration::from_millis(100))
        .await;
}

#[tokio::test]
async fn reload_adds_removes_and_rebinds_listeners_at_runtime() {
    let a = marker_backend(b'A').await;
    let p1 = free_port().await;
    let p2 = free_port().await;
    let p2b = free_port().await;

    let only_l1 = format!(
        "pools:\n  - name: p\n    targets: [\"{a}\"]\n\
         listeners:\n  - name: l1\n    bind: \"{p1}\"\n    pool: p\n"
    );
    // Start with just l1.
    let cfg = parse_str(&only_l1).unwrap();
    let runtime = Runtime::start(Snapshot::from_config(&cfg), Default::default(), 1);
    let handle = runtime.handle();
    tokio::time::sleep(Duration::from_millis(150)).await;

    let hit = |addr: std::net::SocketAddr| async move {
        let mut c = TcpStream::connect(addr).await.ok()?;
        let mut m = [0u8; 1];
        c.read_exact(&mut m).await.ok()?;
        Some(m[0])
    };
    assert_eq!(hit(p1).await, Some(b'A'));
    assert!(hit(p2).await.is_none(), "l2 not configured yet");

    // Reload: add l2 on p2.
    let cfg2 = parse_str(&format!(
        "pools:\n  - name: p\n    targets: [\"{a}\"]\n\
         listeners:\n  - name: l1\n    bind: \"{p1}\"\n    pool: p\n\
         \x20 - name: l2\n    bind: \"{p2}\"\n    pool: p\n"
    ))
    .unwrap();
    handle.store(Snapshot::from_config(&cfg2));
    handle.reconcile_listeners().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(hit(p1).await, Some(b'A'));
    assert_eq!(hit(p2).await, Some(b'A'), "l2 should be live after reload");

    // Reload: rebind l2 p2 -> p2b, and drop... keep l1.
    let cfg3 = parse_str(&format!(
        "pools:\n  - name: p\n    targets: [\"{a}\"]\n\
         listeners:\n  - name: l1\n    bind: \"{p1}\"\n    pool: p\n\
         \x20 - name: l2\n    bind: \"{p2b}\"\n    pool: p\n"
    ))
    .unwrap();
    handle.store(Snapshot::from_config(&cfg3));
    handle.reconcile_listeners().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(hit(p2).await.is_none(), "old l2 bind should be gone");
    assert_eq!(hit(p2b).await, Some(b'A'), "l2 rebound to the new port");

    // Reload: remove l2 entirely.
    let cfg4 = parse_str(&only_l1).unwrap();
    handle.store(Snapshot::from_config(&cfg4));
    handle.reconcile_listeners().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(hit(p2b).await.is_none(), "l2 removed");
    assert_eq!(hit(p1).await, Some(b'A'), "l1 still serving");

    runtime
        .shutdown_with_grace(std::time::Duration::from_millis(100))
        .await;
}

#[tokio::test]
async fn prepends_a_proxy_protocol_v1_header_to_the_backend() {
    // Backend: read one line (the PROXY header), remember it, then echo.
    let backend = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_addr = backend.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<String>();
    tokio::spawn(async move {
        let mut tx = Some(tx);
        loop {
            let (mut s, _) = backend.accept().await.unwrap();
            // A health-check probe connects and closes with no bytes; skip it.
            let mut hdr = Vec::new();
            let mut byte = [0u8; 1];
            let ok = loop {
                match s.read_exact(&mut byte).await {
                    Ok(_) => {
                        hdr.push(byte[0]);
                        if hdr.ends_with(b"\r\n") {
                            break true;
                        }
                    }
                    Err(_) => break false,
                }
            };
            if !ok {
                continue;
            }
            let _ = tx.take().unwrap().send(String::from_utf8(hdr).unwrap());
            let mut buf = [0u8; 4096];
            while let Ok(n) = s.read(&mut buf).await {
                if n == 0 || s.write_all(&buf[..n]).await.is_err() {
                    break;
                }
            }
            return;
        }
    });

    let proxy_addr = {
        let p = TcpListener::bind("127.0.0.1:0").await.unwrap();
        p.local_addr().unwrap()
    };
    let yaml = format!(
        "pools:\n  - name: p\n    targets: [\"{backend_addr}\"]\n    proxy_protocol: v1\n\
         listeners:\n  - name: l\n    bind: \"{proxy_addr}\"\n    pool: p\n"
    );
    let cfg = parse_str(&yaml).unwrap();
    let runtime = Runtime::start(Snapshot::from_config(&cfg), Default::default(), 1);
    tokio::time::sleep(Duration::from_millis(150)).await;

    let mut client = TcpStream::connect(proxy_addr).await.unwrap();
    let client_local = client.local_addr().unwrap();
    client.write_all(b"ping").await.unwrap();
    let mut buf = [0u8; 4];
    client.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"ping");

    let hdr = tokio::time::timeout(Duration::from_secs(1), rx)
        .await
        .unwrap()
        .unwrap();
    let expected = format!(
        "PROXY TCP4 127.0.0.1 127.0.0.1 {} {}\r\n",
        client_local.port(),
        proxy_addr.port()
    );
    assert_eq!(hdr, expected);

    drop(client);
    runtime
        .shutdown_with_grace(std::time::Duration::from_millis(100))
        .await;
}

#[tokio::test]
async fn acl_deny_drops_the_connection_before_routing() {
    // Echo backend that should never be reached.
    let backend = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_addr = backend.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = backend.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                while let Ok(n) = s.read(&mut buf).await {
                    if n == 0 || s.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });

    let proxy_addr = {
        let p = TcpListener::bind("127.0.0.1:0").await.unwrap();
        p.local_addr().unwrap()
    };
    let yaml = format!(
        "pools:\n  - name: p\n    targets: [\"{backend_addr}\"]\n\
         listeners:\n  - name: l\n    bind: \"{proxy_addr}\"\n    pool: p\n    deny: [\"127.0.0.1/32\"]\n"
    );
    let cfg = parse_str(&yaml).unwrap();
    let runtime = Runtime::start(Snapshot::from_config(&cfg), Default::default(), 1);
    tokio::time::sleep(Duration::from_millis(150)).await;

    // The proxy accepts the TCP connection then drops it without connecting a
    // backend: the client sees EOF and never gets its bytes echoed.
    let mut client = TcpStream::connect(proxy_addr).await.unwrap();
    let _ = client.write_all(b"hello").await;
    let mut buf = [0u8; 5];
    let read = tokio::time::timeout(Duration::from_secs(1), client.read(&mut buf)).await;
    match read {
        Ok(Ok(0)) => {}  // clean EOF
        Ok(Err(_)) => {} // or connection reset
        other => panic!("expected the connection to be dropped, got {other:?}"),
    }

    runtime
        .shutdown_with_grace(std::time::Duration::from_millis(100))
        .await;
}

#[tokio::test]
async fn rate_limit_drops_connections_past_the_burst() {
    // Echo backend.
    let backend = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_addr = backend.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = backend.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                while let Ok(n) = s.read(&mut buf).await {
                    if n == 0 || s.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });

    let proxy_addr = {
        let p = TcpListener::bind("127.0.0.1:0").await.unwrap();
        p.local_addr().unwrap()
    };
    // 1 permit/sec, burst 2: at most 2 of a quick run of connections get through.
    let yaml = format!(
        "pools:\n  - name: p\n    targets: [\"{backend_addr}\"]\n\
         listeners:\n  - name: l\n    bind: \"{proxy_addr}\"\n    pool: p\n\
         \x20   rate_limit:\n      per_ip: {{ rate: 1, burst: 2 }}\n"
    );
    let cfg = parse_str(&yaml).unwrap();
    let runtime = Runtime::start(Snapshot::from_config(&cfg), Default::default(), 1);
    tokio::time::sleep(Duration::from_millis(150)).await;

    let mut forwarded = 0;
    for _ in 0..8 {
        let Ok(mut c) = TcpStream::connect(proxy_addr).await else {
            continue;
        };
        if c.write_all(b"x").await.is_err() {
            continue;
        }
        let mut b = [0u8; 1];
        if let Ok(Ok(_)) =
            tokio::time::timeout(Duration::from_millis(200), c.read_exact(&mut b)).await
        {
            forwarded += 1;
        }
    }
    assert!(
        (1..=3).contains(&forwarded),
        "expected ~2 forwarded (burst), got {forwarded}"
    );

    runtime
        .shutdown_with_grace(std::time::Duration::from_millis(100))
        .await;
}

#[tokio::test]
async fn global_max_connections_caps_live_tcp() {
    let backend = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_addr = backend.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = backend.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                while let Ok(n) = s.read(&mut buf).await {
                    if n == 0 || s.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });

    let proxy_addr = {
        let p = TcpListener::bind("127.0.0.1:0").await.unwrap();
        p.local_addr().unwrap()
    };
    let yaml = format!(
        "settings:\n  limits:\n    max_connections: 2\n\
         pools:\n  - name: p\n    targets: [\"{backend_addr}\"]\n\
         listeners:\n  - name: l\n    bind: \"{proxy_addr}\"\n    pool: p\n"
    );
    let cfg = parse_str(&yaml).unwrap();
    let runtime = Runtime::start(Snapshot::from_config(&cfg), Default::default(), 1);
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Two long-lived connections fill the cap.
    let mut c1 = TcpStream::connect(proxy_addr).await.unwrap();
    let mut c2 = TcpStream::connect(proxy_addr).await.unwrap();
    for c in [&mut c1, &mut c2] {
        c.write_all(b"z").await.unwrap();
        let mut b = [0u8; 1];
        c.read_exact(&mut b).await.unwrap();
    }

    // The third is accepted at the socket level but dropped before a backend
    // connect: no echo comes back.
    let mut c3 = TcpStream::connect(proxy_addr).await.unwrap();
    let _ = c3.write_all(b"z").await;
    let mut b = [0u8; 1];
    let r = tokio::time::timeout(Duration::from_millis(400), c3.read(&mut b)).await;
    assert!(
        matches!(r, Ok(Ok(0)) | Ok(Err(_))),
        "3rd connection should be dropped by the cap, got {r:?}"
    );

    // Free a slot; a new connection now gets through.
    drop(c1);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut c4 = TcpStream::connect(proxy_addr).await.unwrap();
    c4.write_all(b"z").await.unwrap();
    let mut b = [0u8; 1];
    tokio::time::timeout(Duration::from_millis(400), c4.read_exact(&mut b))
        .await
        .expect("slot freed, 4th connection should be forwarded")
        .unwrap();

    drop((c2, c4));
    runtime
        .shutdown_with_grace(std::time::Duration::from_millis(100))
        .await;
}

#[tokio::test]
async fn geo_filter_denies_unlisted_country() {
    let backend = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_addr = backend.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = backend.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 64];
                while let Ok(n) = s.read(&mut buf).await {
                    if n == 0 || s.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    let proxy_addr = {
        let p = TcpListener::bind("127.0.0.1:0").await.unwrap();
        p.local_addr().unwrap()
    };
    // `allow: [SE]` — the loopback client resolves to no country in the test DB,
    // so a non-empty allow list fails it closed.
    let yaml = format!(
        "settings:\n  geo_db: \"{}/tests/data/GeoIP2-Country-Test.mmdb\"\n\
         pools:\n  - name: p\n    targets: [\"{backend_addr}\"]\n\
         listeners:\n  - name: l\n    bind: \"{proxy_addr}\"\n    pool: p\n\
         \x20   geo:\n      allow: [\"SE\"]\n",
        env!("CARGO_MANIFEST_DIR")
    );
    let cfg = parse_str(&yaml).unwrap();
    let geo = gsp_core::GeoDb::open(cfg.geo_db.as_ref().unwrap()).unwrap();
    let runtime = gsp_core::Runtime::start_with_geo(
        Snapshot::from_config(&cfg),
        Default::default(),
        Some(geo),
        1,
    );
    tokio::time::sleep(Duration::from_millis(150)).await;

    let mut c = TcpStream::connect(proxy_addr).await.unwrap();
    let _ = c.write_all(b"hi").await;
    let mut buf = [0u8; 2];
    let r = tokio::time::timeout(Duration::from_secs(1), c.read(&mut buf)).await;
    match r {
        Ok(Ok(0)) | Ok(Err(_)) => {}
        other => panic!("geo-denied connection should be dropped, got {other:?}"),
    }

    runtime
        .shutdown_with_grace(std::time::Duration::from_millis(100))
        .await;
}

#[tokio::test]
async fn per_source_cap_limits_concurrent_connections_from_one_ip() {
    let backend = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_addr = backend.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = backend.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 64];
                while let Ok(n) = s.read(&mut buf).await {
                    if n == 0 || s.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    let proxy_addr = {
        let p = TcpListener::bind("127.0.0.1:0").await.unwrap();
        p.local_addr().unwrap()
    };
    let yaml = format!(
        "pools:\n  - name: p\n    targets: [\"{backend_addr}\"]\n\
         listeners:\n  - name: l\n    bind: \"{proxy_addr}\"\n    pool: p\n\
         \x20   per_source:\n      max_per_ip: 2\n"
    );
    let cfg = parse_str(&yaml).unwrap();
    let runtime = Runtime::start(Snapshot::from_config(&cfg), Default::default(), 1);
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Two long-lived connections from loopback fill the per-IP cap.
    let mut c1 = TcpStream::connect(proxy_addr).await.unwrap();
    let mut c2 = TcpStream::connect(proxy_addr).await.unwrap();
    for c in [&mut c1, &mut c2] {
        c.write_all(b"z").await.unwrap();
        let mut b = [0u8; 1];
        c.read_exact(&mut b).await.unwrap();
    }

    // Third from the same IP is dropped before a backend connect.
    let mut c3 = TcpStream::connect(proxy_addr).await.unwrap();
    let _ = c3.write_all(b"z").await;
    let mut b = [0u8; 1];
    let r = tokio::time::timeout(Duration::from_millis(400), c3.read(&mut b)).await;
    assert!(
        matches!(r, Ok(Ok(0)) | Ok(Err(_))),
        "3rd concurrent connection should be dropped, got {r:?}"
    );

    // Free one slot; a new connection now gets through.
    drop(c1);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut c4 = TcpStream::connect(proxy_addr).await.unwrap();
    c4.write_all(b"z").await.unwrap();
    let mut b = [0u8; 1];
    tokio::time::timeout(Duration::from_millis(400), c4.read_exact(&mut b))
        .await
        .expect("slot freed, new connection should be forwarded")
        .unwrap();

    drop((c2, c4));
    runtime
        .shutdown_with_grace(std::time::Duration::from_millis(100))
        .await;
}
