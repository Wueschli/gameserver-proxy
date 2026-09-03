//! Regression tests for the "abuse as an amplifier" checklist in
//! `docs/07-security-ddos.md`:
//!
//! - no reply to a client without an established session;
//! - no error / unsolicited reply when a datagram is dropped (routing, ACL,
//!   rate limit, first-packet gate);
//! - the proxy adds no payload of its own toward the client (reply size is
//!   exactly what the backend sent);
//! - the rate limit is applied before any state change (no session, no forward
//!   to the backend) for the datagrams it rejects.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::net::UdpSocket;

use gsp_config::parse_str;
use gsp_core::{Runtime, Snapshot};

fn free_udp_addr() -> std::net::SocketAddr {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

/// A UDP echo backend that counts the datagrams it receives.
async fn counting_echo_backend() -> (std::net::SocketAddr, Arc<AtomicUsize>) {
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = sock.local_addr().unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let c = count.clone();
    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        while let Ok((n, peer)) = sock.recv_from(&mut buf).await {
            c.fetch_add(1, Ordering::SeqCst);
            let _ = sock.send_to(&buf[..n], peer).await;
        }
    });
    (addr, count)
}

async fn start(yaml: &str) -> Runtime {
    let cfg = parse_str(yaml).unwrap();
    let rt = Runtime::start(Snapshot::from_config(&cfg), Default::default(), 1);
    tokio::time::sleep(Duration::from_millis(150)).await;
    rt
}

/// A silent client and an uninvolved third party never hear from the proxy, and
/// an established session gets exactly one reply per request (no duplicates,
/// nothing unsolicited).
#[tokio::test]
async fn no_unsolicited_or_duplicated_replies() {
    let (backend, _n) = counting_echo_backend().await;
    let proxy = free_udp_addr();
    let rt = start(&format!(
        "pools:\n  - name: p\n    targets: [\"{backend}\"]\n\
         listeners:\n  - name: l\n    bind: \"{proxy}\"\n    protocol: udp\n    pool: p\n"
    ))
    .await;

    // A client that connects but never sends must receive nothing.
    let silent = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    silent.connect(proxy).await.unwrap();
    let mut buf = [0u8; 64];
    assert!(
        tokio::time::timeout(Duration::from_millis(200), silent.recv(&mut buf))
            .await
            .is_err(),
        "silent client got an unsolicited datagram"
    );

    // An active session: one request -> exactly one reply, and no more.
    let active = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    active.connect(proxy).await.unwrap();
    active.send(b"ping").await.unwrap();
    let n = tokio::time::timeout(Duration::from_millis(500), active.recv(&mut buf))
        .await
        .expect("established session should be answered once")
        .unwrap();
    assert_eq!(&buf[..n], b"ping");
    assert!(
        tokio::time::timeout(Duration::from_millis(200), active.recv(&mut buf))
            .await
            .is_err(),
        "a single request produced more than one reply"
    );

    // A bystander that never contacted the proxy hears nothing while the
    // session above is live.
    let bystander = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(200), bystander.recv(&mut buf))
            .await
            .is_err(),
        "a bystander received a datagram from the proxy"
    );

    rt.shutdown_with_grace(Duration::from_millis(100)).await;
}

/// A datagram that matches no route is dropped in silence — no error datagram
/// back to the (possibly spoofed) sender, and nothing forwarded to a backend.
#[tokio::test]
async fn dropped_datagrams_get_no_error_reply() {
    let (backend, n) = counting_echo_backend().await;
    let proxy = free_udp_addr();
    // Only 0xFFFFFFFF-prefixed datagrams route anywhere; everything else is
    // `no_route`.
    let rt = start(&format!(
        r#"
pools:
  - name: p
    targets: ["{backend}"]
listeners:
  - name: l
    bind: "{proxy}"
    protocol: udp
    routes:
      - match: {{ type: first_bytes, prefix: "hex:ffffffff" }}
        action: {{ pool: p }}
"#
    ))
    .await;

    let c = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    c.connect(proxy).await.unwrap();
    c.send(b"no route for this").await.unwrap();

    let mut buf = [0u8; 64];
    assert!(
        tokio::time::timeout(Duration::from_millis(250), c.recv(&mut buf))
            .await
            .is_err(),
        "an unroutable datagram drew a reply"
    );
    assert_eq!(
        n.load(Ordering::SeqCst),
        0,
        "nothing should reach the backend"
    );

    rt.shutdown_with_grace(Duration::from_millis(100)).await;
}

/// The proxy forwards backend bytes verbatim and prepends nothing toward the
/// client — the reply is exactly the backend's payload, so it can never be an
/// amplification vector.
#[tokio::test]
async fn reply_is_exactly_the_backend_payload() {
    // Backend that answers with a *smaller* payload than the request.
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let backend = sock.local_addr().unwrap();
    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        while let Ok((_n, peer)) = sock.recv_from(&mut buf).await {
            let _ = sock.send_to(b"ok", peer).await;
        }
    });
    let proxy = free_udp_addr();
    let rt = start(&format!(
        "pools:\n  - name: p\n    targets: [\"{backend}\"]\n\
         listeners:\n  - name: l\n    bind: \"{proxy}\"\n    protocol: udp\n    pool: p\n"
    ))
    .await;

    let c = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    c.connect(proxy).await.unwrap();
    c.send(&[0u8; 400]).await.unwrap();

    let mut buf = [0u8; 64];
    let n = tokio::time::timeout(Duration::from_millis(500), c.recv(&mut buf))
        .await
        .expect("reply expected")
        .unwrap();
    assert_eq!(
        &buf[..n],
        b"ok",
        "proxy must not add bytes toward the client"
    );

    rt.shutdown_with_grace(Duration::from_millis(100)).await;
}

/// With a low `rate_limit`, the datagrams the limiter rejects never create a
/// session and are never forwarded to the backend — the limit is enforced
/// before any state change.
#[tokio::test]
async fn rate_limit_is_enforced_before_any_state_change() {
    let (backend, n) = counting_echo_backend().await;
    let proxy = free_udp_addr();
    // burst 2 new sessions per source IP; loopback shares one IP.
    let rt = start(&format!(
        "pools:\n  - name: p\n    targets: [\"{backend}\"]\n\
         listeners:\n  - name: l\n    bind: \"{proxy}\"\n    protocol: udp\n    pool: p\n\
         \x20   rate_limit:\n      per_ip: {{ rate: 1, burst: 2 }}\n"
    ))
    .await;

    // Six distinct client sockets (distinct ephemeral ports => distinct
    // sessions), each sending one datagram in a tight burst.
    let mut replies = 0;
    for _ in 0..6 {
        let c = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        c.connect(proxy).await.unwrap();
        c.send(b"x").await.unwrap();
        let mut buf = [0u8; 16];
        if tokio::time::timeout(Duration::from_millis(150), c.recv(&mut buf))
            .await
            .is_ok()
        {
            replies += 1;
        }
    }

    let forwarded = n.load(Ordering::SeqCst);
    assert!(
        (1..=3).contains(&forwarded),
        "only the burst should reach the backend, got {forwarded}"
    );
    assert!(
        replies <= 3,
        "only the burst should be answered, got {replies}"
    );

    rt.shutdown_with_grace(Duration::from_millis(100)).await;
}
