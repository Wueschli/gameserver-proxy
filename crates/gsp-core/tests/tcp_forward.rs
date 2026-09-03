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
    let runtime = Runtime::start(snapshot, 1);

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
    let runtime = Runtime::start(Snapshot::from_config(&cfg), 1);
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
    let runtime = Runtime::start(Snapshot::from_config(&cfg), 1);
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
