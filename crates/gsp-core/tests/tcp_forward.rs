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
