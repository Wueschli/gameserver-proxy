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
    runtime.shutdown().await;
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
    runtime.shutdown().await;
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

    runtime.shutdown().await;
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

    runtime.shutdown().await;
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

    runtime.shutdown().await;
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

    runtime.shutdown().await;
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

    runtime.shutdown().await;
}
