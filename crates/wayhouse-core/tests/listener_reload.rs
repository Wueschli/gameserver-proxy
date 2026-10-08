//! Listener reload edge cases: a cancelled reload must not strand the groups it
//! was stopping (#260), and a UDP hand-off must not wait on workers that keep
//! their port (#258).

use std::time::{Duration, Instant};

use tokio::net::UdpSocket;

use wayhouse_config::parse_str;
use wayhouse_core::{Runtime, Snapshot};

fn free_tcp_addr() -> std::net::SocketAddr {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

fn free_udp_addr() -> std::net::SocketAddr {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

/// #260: a reload that is cancelled while it awaits the groups it stopped must
/// leave them with the manager, so a later shutdown still stops every one.
/// Before the fix the local `Vec` was dropped with the future, which detached
/// every group that had not been signalled yet and left its port bound.
#[tokio::test]
async fn a_cancelled_reload_leaves_stopped_groups_to_stop_all() {
    let (a, b, c) = (free_tcp_addr(), free_tcp_addr(), free_tcp_addr());
    let cfg1 = parse_str(&format!(
        r#"
pools:
  - name: p
    targets: ["127.0.0.1:9"]
    health_check:
      type: none
listeners:
  - name: a
    bind: "{a}"
    pool: p
  - name: b
    bind: "{b}"
    pool: p
  - name: c
    bind: "{c}"
    pool: p
"#
    ))
    .unwrap();
    let runtime = Runtime::start(Snapshot::from_config(&cfg1), std::sync::Arc::default(), 1);
    let handle = runtime.handle();

    // Only `c` stays; `a` and `b` are stopped.
    let cfg2 = parse_str(&format!(
        r#"
pools:
  - name: p
    targets: ["127.0.0.1:9"]
    health_check:
      type: none
listeners:
  - name: c
    bind: "{c}"
    pool: p
"#
    ))
    .unwrap();
    handle.store(Snapshot::build_with_overlay(
        &cfg2,
        Some(&handle.current()),
        handle.backend_overlay(),
    ));
    // Cancel the reload at its first await.
    let _ = tokio::time::timeout(Duration::ZERO, handle.reconcile_listeners()).await;
    runtime.shutdown().await;

    for addr in [a, b] {
        let mut bound = false;
        for _ in 0..40 {
            if std::net::TcpListener::bind(addr).is_ok() {
                bound = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(bound, "a worker still holds {addr} after shutdown");
    }
}

/// #258: of a UDP group with two binds, only the one the successor takes over
/// leaves at once; the other keeps draining its live session. The reload must
/// not wait the full hand-off limit on that worker.
#[tokio::test]
async fn a_hand_off_does_not_wait_for_workers_that_keep_their_port() {
    // Two adjacent free ports: the group binds both, the successor only `keep`.
    let (keep, drop_) = loop {
        let a = free_udp_addr();
        if a.port() < u16::MAX
            && std::net::UdpSocket::bind(std::net::SocketAddr::new(a.ip(), a.port() + 1)).is_ok()
        {
            break (a, std::net::SocketAddr::new(a.ip(), a.port() + 1));
        }
    };
    let backend = free_udp_addr();
    let echo = UdpSocket::bind(backend).await.unwrap();
    tokio::spawn(async move {
        let mut buf = [0u8; 64];
        while let Ok((n, from)) = echo.recv_from(&mut buf).await {
            let _ = echo.send_to(&buf[..n], from).await;
        }
    });
    let yaml = |binds: &str, extra: &str| {
        format!(
            r#"
pools:
  - name: p
    targets: ["{backend}"]
    health_check:
      type: none
listeners:
  - name: l
    bind: "{binds}"
    protocol: udp
    pool: p
{extra}"#
        )
    };
    let cfg1 = parse_str(&yaml(&format!("{keep}-{}", drop_.port()), "")).unwrap();
    let runtime = Runtime::start(Snapshot::from_config(&cfg1), std::sync::Arc::default(), 1);
    let handle = runtime.handle();

    // A chatty client on the port the successor will not take over.
    let client = std::sync::Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    client.connect(drop_).await.unwrap();
    let mut buf = [0u8; 64];
    for _ in 0..40 {
        if client.send(b"hb").await.is_ok()
            && tokio::time::timeout(Duration::from_millis(250), client.recv(&mut buf))
                .await
                .is_ok_and(|r| r.is_ok())
        {
            break;
        }
    }
    let pinger = {
        let client = client.clone();
        tokio::spawn(async move {
            loop {
                let _ = client.send(b"hb").await;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
    };

    // The successor keeps only `keep`, with a changed rate limit.
    let cfg2 = parse_str(&yaml(
        &keep.to_string(),
        "    rate_limit:\n      per_ip: { rate: 1000, burst: 1000 }\n",
    ))
    .unwrap();
    handle.store(Snapshot::build_with_overlay(
        &cfg2,
        Some(&handle.current()),
        handle.backend_overlay(),
    ));
    let t = Instant::now();
    let out = handle.reconcile_listeners().await;
    let took = t.elapsed();
    pinger.abort();
    assert_eq!(out.stopped, 1);
    assert!(
        took < Duration::from_millis(400),
        "reload waited {took:?} on a worker that keeps its port"
    );
}
