//! Refused and evicted handshakes are counted. One test per binary: it installs
//! the process-global metrics recorder.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use tokio::net::{TcpSocket, TcpStream};
use wayhouse_http::tls::{HandshakeLimits, ReloadingCert, TlsFiles, TlsListener};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

async fn idle_from(src: &str, addr: SocketAddr) -> TcpStream {
    let socket = TcpSocket::new_v4().unwrap();
    socket.bind(format!("{src}:0").parse().unwrap()).unwrap();
    socket.connect(addr).await.unwrap()
}

/// The value of the sample whose line starts with `series`, 0 if absent.
fn counter(rendered: &str, series: &str) -> u64 {
    rendered
        .lines()
        .find_map(|l| l.strip_prefix(series))
        .map_or(0, |v| v.trim().parse().unwrap())
}

#[tokio::test]
async fn refused_and_evicted_handshakes_are_counted() {
    let prom = metrics_exporter_prometheus::PrometheusBuilder::new()
        .install_recorder()
        .unwrap();
    let cert = ReloadingCert::new(TlsFiles::new(fixture("leaf.pem"), fixture("leaf.key"))).unwrap();
    let limits = HandshakeLimits {
        max_pending: 3,
        max_pending_per_source: 2,
        ..HandshakeLimits::default()
    };
    let _listener = TlsListener::bind_with("127.0.0.1:0".parse().unwrap(), cert, limits)
        .await
        .unwrap();
    let addr = axum::serve::Listener::local_addr(&_listener).unwrap();

    // 127.0.0.3: two connects pending, the third is over its per-source cap.
    let mut held = Vec::new();
    for _ in 0..3 {
        held.push(idle_from("127.0.0.3", addr).await);
    }
    // 127.0.0.4, .5, .6 add pending handshakes past the global cap of 3, so the
    // oldest are evicted.
    for src in ["127.0.0.4", "127.0.0.5", "127.0.0.6"] {
        held.push(idle_from(src, addr).await);
    }

    // The accept loop counts asynchronously: poll until both show, or give up.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let (mut per_source, mut evicted, mut out);
    loop {
        out = prom.render();
        per_source = counter(
            &out,
            "wayhouse_tls_handshakes_refused_total{reason=\"per_source\"}",
        );
        evicted = counter(&out, "wayhouse_tls_handshakes_evicted_total");
        if (per_source >= 1 && evicted >= 1) || std::time::Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(per_source >= 1, "no per-source refusal counted:\n{out}");
    assert!(evicted >= 1, "no eviction counted:\n{out}");
    drop(held);
}
