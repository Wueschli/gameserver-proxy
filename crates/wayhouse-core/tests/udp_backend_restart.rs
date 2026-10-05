//! A backend that crashes and comes back on the same address must keep serving
//! the clients whose sessions were open when it went down (#168): the ICMP
//! port-unreachable of the dead window must not kill the session's reply pump.

use std::time::Duration;
use tokio::net::UdpSocket;
use wayhouse_config::parse_str;
use wayhouse_core::{Runtime, Snapshot};

fn free_udp_addr() -> std::net::SocketAddr {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

fn spawn_echo(addr: std::net::SocketAddr) -> tokio::task::JoinHandle<()> {
    let s = std::net::UdpSocket::bind(addr).unwrap();
    s.set_nonblocking(true).unwrap();
    tokio::spawn(async move {
        let sock = UdpSocket::from_std(s).unwrap();
        let mut buf = [0u8; 2048];
        while let Ok((n, peer)) = sock.recv_from(&mut buf).await {
            let _ = sock.send_to(&buf[..n], peer).await;
        }
    })
}

#[tokio::test]
async fn session_survives_backend_restart() {
    let backend = free_udp_addr();
    let proxy_addr = free_udp_addr();
    let echo = spawn_echo(backend);
    let yaml = format!(
        r#"
pools:
  - name: p
    targets: ["{backend}"]
    health_check: {{ type: none }}
listeners:
  - name: l
    bind: "{proxy_addr}"
    protocol: udp
    pool: p
"#
    );
    let cfg = parse_str(&yaml).unwrap();
    let runtime = Runtime::start(Snapshot::from_config(&cfg), std::sync::Arc::default(), 1);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client.connect(proxy_addr).await.unwrap();
    let mut buf = [0u8; 64];
    let mut warm = false;
    for _ in 0..20 {
        client.send(b"hello").await.unwrap();
        if let Ok(Ok(_)) =
            tokio::time::timeout(Duration::from_millis(200), client.recv(&mut buf)).await
        {
            warm = true;
            break;
        }
    }
    assert!(warm, "session never came up");

    // Backend dies; the heartbeat keeps flowing and draws ICMP errors.
    echo.abort();
    let _ = echo.await;
    for _ in 0..5 {
        let _ = client.send(b"hb").await;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Backend returns on the same address; the old client keeps its source port.
    let _echo2 = spawn_echo(backend);
    tokio::time::sleep(Duration::from_millis(100)).await;
    while let Ok(Ok(_)) =
        tokio::time::timeout(Duration::from_millis(50), client.recv(&mut buf)).await
    {}
    let mut got = false;
    for _ in 0..30 {
        let _ = client.send(b"after-restart").await;
        if let Ok(Ok(n)) =
            tokio::time::timeout(Duration::from_millis(100), client.recv(&mut buf)).await
        {
            if &buf[..n] == b"after-restart" {
                got = true;
                break;
            }
        }
    }
    assert!(
        got,
        "no reply after the backend restarted on the same session"
    );
    runtime
        .shutdown_with_grace(Duration::from_millis(100))
        .await;
}
