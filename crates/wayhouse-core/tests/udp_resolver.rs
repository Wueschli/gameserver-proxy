//! A slow external resolver must not stall the UDP receive loop: while one new
//! session's route is pending, established sessions keep flowing and extra
//! datagrams for the pending session are buffered, not dropped.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::net::UdpSocket;
use tokio::sync::Notify;

use wayhouse_config::{parse_str, OnError};
use wayhouse_core::{
    Resolution, ResolveError, ResolveRequest, Resolver, Resolvers, Runtime, Snapshot,
};

/// Answers `pool: p` at once, except for first datagrams starting with `slow`,
/// which wait on `release`.
struct Gated {
    release: Arc<Notify>,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Resolver for Gated {
    fn name(&self) -> &str {
        "gated"
    }
    fn on_error(&self) -> OnError {
        OnError::Reject
    }
    async fn resolve(&self, req: ResolveRequest) -> Result<Resolution, ResolveError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if req.first_bytes.starts_with(b"fail") {
            return Err(ResolveError::Failed("boom".into()));
        }
        if req.first_bytes.starts_with(b"slow") {
            self.release.notified().await;
        }
        Ok(Resolution {
            pool: Some("p".into()),
            ..Default::default()
        })
    }
}

async fn echo_backend() -> std::net::SocketAddr {
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = sock.local_addr().unwrap();
    tokio::spawn(async move {
        let mut buf = [0u8; 65536];
        while let Ok((n, peer)) = sock.recv_from(&mut buf).await {
            let _ = sock.send_to(&buf[..n], peer).await;
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

async fn recv(client: &UdpSocket, wait: Duration) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; 65536];
    let n = tokio::time::timeout(wait, client.recv(&mut buf))
        .await
        .ok()?
        .unwrap();
    Some(buf[..n].to_vec())
}

/// Start a one-worker proxy whose only route is the gated resolver.
async fn start_gated() -> (Runtime, std::net::SocketAddr, Arc<Notify>, Arc<AtomicUsize>) {
    let backend = echo_backend().await;
    let proxy = free_udp_addr();
    let yaml = format!(
        r#"
pools:
  - {{ name: p, targets: ["{backend}"] }}
resolvers:
  - {{ name: gated, endpoint: "http://x" }}
listeners:
  - name: l
    bind: "{proxy}"
    protocol: udp
    routes:
      - {{ match: {{ type: always }}, action: {{ resolver: gated }} }}
"#
    );
    let cfg = parse_str(&yaml).unwrap();
    let release = Arc::new(Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let resolvers = Resolvers::new();
    resolvers.insert(
        "gated".into(),
        Arc::new(Gated {
            release: release.clone(),
            calls: calls.clone(),
        }),
    );
    // One worker: the slow session and the established one share a recv loop.
    let runtime = Runtime::start(Snapshot::from_config(&cfg), Arc::new(resolvers), 1);
    tokio::time::sleep(Duration::from_millis(150)).await;
    (runtime, proxy, release, calls)
}

#[tokio::test]
async fn pending_resolution_does_not_stall_established_sessions() {
    let (runtime, proxy, release, calls) = start_gated().await;

    let fast = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    fast.connect(proxy).await.unwrap();
    fast.send(b"fast-1").await.unwrap();
    assert_eq!(
        recv(&fast, Duration::from_secs(1)).await.unwrap(),
        b"fast-1"
    );

    // A new client whose resolution is held open, sending three datagrams.
    let slow = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    slow.connect(proxy).await.unwrap();
    slow.send(b"slow-1").await.unwrap();
    slow.send(b"more-2").await.unwrap();
    slow.send(b"more-3").await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // The established session still flows while that resolution is pending.
    fast.send(b"fast-2").await.unwrap();
    assert_eq!(
        recv(&fast, Duration::from_millis(500)).await.as_deref(),
        Some(&b"fast-2"[..]),
        "established session stalled behind a pending resolver call"
    );
    assert!(recv(&slow, Duration::from_millis(100)).await.is_none());

    // Releasing the resolver flushes every buffered datagram, in order, with
    // exactly one resolver call for the pending session.
    release.notify_waiters();
    for want in [&b"slow-1"[..], b"more-2", b"more-3"] {
        assert_eq!(
            recv(&slow, Duration::from_secs(1)).await.as_deref(),
            Some(want)
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2, "fast-1 + one slow call");

    runtime
        .shutdown_with_grace(Duration::from_millis(100))
        .await;
}

/// Datagrams beyond the per-session pending buffer are dropped, not queued
/// without bound.
#[tokio::test]
async fn pending_buffer_is_bounded() {
    let (runtime, proxy, release, _calls) = start_gated().await;
    let slow = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    slow.connect(proxy).await.unwrap();
    slow.send(b"slow-0").await.unwrap();
    for i in 1..8u8 {
        slow.send(&[b'm', i]).await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    release.notify_waiters();

    let mut got = 0;
    while recv(&slow, Duration::from_millis(300)).await.is_some() {
        got += 1;
    }
    assert_eq!(got, 4, "first datagram plus three buffered ones");

    runtime
        .shutdown_with_grace(Duration::from_millis(100))
        .await;
}

/// The total bytes buffered across pending sessions are capped per worker.
#[tokio::test]
async fn pending_bytes_are_capped_per_worker() {
    let (runtime, proxy, release, _calls) = start_gated().await;
    let mut clients = Vec::new();
    for _ in 0..30 {
        let c = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        c.connect(proxy).await.unwrap();
        let mut payload = vec![0u8; 40_000];
        payload[..4].copy_from_slice(b"slow");
        c.send(&payload).await.unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        clients.push(c);
    }
    release.notify_waiters();

    let mut answered = 0;
    for c in &clients {
        if recv(c, Duration::from_millis(300)).await.is_some() {
            answered += 1;
        }
    }
    // 1 MiB cap / 40 kB = 26 sessions fit; the rest are dropped, not queued.
    assert_eq!(answered, 26);

    runtime
        .shutdown_with_grace(Duration::from_millis(100))
        .await;
}

/// A failed resolve drops the buffered datagrams and frees the pending slot, so
/// the same client can open a session afterwards.
#[tokio::test]
async fn failed_resolve_drops_buffered_packets_and_frees_the_slot() {
    let (runtime, proxy, _release, calls) = start_gated().await;
    let c = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    c.connect(proxy).await.unwrap();
    c.send(b"fail-1").await.unwrap();
    c.send(b"more-2").await.unwrap();
    assert!(recv(&c, Duration::from_millis(300)).await.is_none());

    c.send(b"ok-3").await.unwrap();
    assert_eq!(recv(&c, Duration::from_secs(1)).await.unwrap(), b"ok-3");
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    runtime
        .shutdown_with_grace(Duration::from_millis(100))
        .await;
}
