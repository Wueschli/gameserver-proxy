//! Native TLS: `TlsListener` under a real `axum::serve`, loopback only.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::routing::get;
use axum::Router;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpSocket, TcpStream};
use wayhouse_http::tls::{spawn_reloader, HandshakeLimits, ReloadingCert, TlsFiles, TlsListener};
use wayhouse_http::{builder_with, load_ca_file};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

async fn serve(files: TlsFiles) -> (SocketAddr, Arc<ReloadingCert>) {
    serve_with(files, HandshakeLimits::default()).await
}

async fn serve_with(files: TlsFiles, limits: HandshakeLimits) -> (SocketAddr, Arc<ReloadingCert>) {
    let cert = ReloadingCert::new(files).unwrap();
    let listener = TlsListener::bind_with("127.0.0.1:0".parse().unwrap(), cert.clone(), limits)
        .await
        .unwrap();
    let addr = axum::serve::Listener::local_addr(&listener).unwrap();
    let app = Router::new().route("/", get(|| async { "ok" }));
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, cert)
}

fn fixture_files() -> TlsFiles {
    TlsFiles::new(fixture("leaf.pem"), fixture("leaf.key"))
}

async fn get_ok(ca: &str, addr: SocketAddr) {
    let client = builder_with(&load_ca_file(&fixture(ca)).unwrap())
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let resp = client
        .get(format!("https://localhost:{}/", addr.port()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "ok");
}

#[tokio::test]
async fn serves_https_to_a_client_that_trusts_the_ca() {
    let (addr, _) = serve(fixture_files()).await;
    get_ok("ca.pem", addr).await;
}

/// `leaf3` is signed by an intermediate that only `ca3` signed: a client trusting
/// `ca3` alone verifies it only if the server sends the intermediate as well.
#[tokio::test]
async fn serves_a_chain_file() {
    let dir = tempfile::tempdir().unwrap();
    let chain = dir.path().join("chain.pem");
    let mut pem = std::fs::read(fixture("leaf3.pem")).unwrap();
    pem.extend(std::fs::read(fixture("inter.pem")).unwrap());
    std::fs::write(&chain, pem).unwrap();
    let (addr, cert) = serve(TlsFiles::new(chain, fixture("leaf3.key"))).await;
    assert_eq!(cert.current().cert.len(), 2);
    get_ok("ca3.pem", addr).await;

    // The control: leaf only, and the same client cannot verify it.
    let (addr, _) = serve(TlsFiles::new(fixture("leaf3.pem"), fixture("leaf3.key"))).await;
    let client = builder_with(&load_ca_file(&fixture("ca3.pem")).unwrap())
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let err = client
        .get(format!("https://localhost:{}/", addr.port()))
        .send()
        .await
        .expect_err("verified a leaf without its intermediate");
    let chain = wayhouse_http::error_chain(&err);
    assert!(chain.contains("UnknownIssuer"), "{chain}");
}

#[tokio::test]
async fn a_stalled_handshake_does_not_block_others() {
    let (addr, _) = serve(fixture_files()).await;
    let _stalled = TcpStream::connect(addr).await.unwrap(); // never says hello
    tokio::time::timeout(Duration::from_secs(2), get_ok("ca.pem", addr))
        .await
        .expect("a second client was blocked behind the stalled handshake");
}

#[tokio::test]
async fn plain_http_on_the_tls_port_does_not_break_the_server() {
    let (addr, _) = serve(fixture_files()).await;
    let mut plain = TcpStream::connect(addr).await.unwrap();
    plain
        .write_all(b"GET / HTTP/1.1\r\nhost: x\r\n\r\n")
        .await
        .unwrap();
    // The server drops that connection (EOF or reset, either is fine), rather than
    // leaving it open.
    let mut buf = Vec::new();
    let _eof_or_reset = tokio::time::timeout(Duration::from_secs(2), plain.read_to_end(&mut buf))
        .await
        .expect("the plain-HTTP connection was left open");
    get_ok("ca.pem", addr).await;
}

#[tokio::test]
async fn the_reloader_picks_up_new_files() {
    let dir = tempfile::tempdir().unwrap();
    let files = TlsFiles::new(dir.path().join("cert.pem"), dir.path().join("key.pem"));
    std::fs::copy(fixture("leaf.pem"), &files.cert).unwrap();
    std::fs::copy(fixture("leaf.key"), &files.key).unwrap();
    let (addr, cert) = serve(files.clone()).await;
    let _reloader = spawn_reloader(cert, Duration::from_millis(100));
    // No wait for the mtime to move on: the stamp also compares size (leaf2.pem is
    // longer than leaf.pem) and ctime.
    std::fs::copy(fixture("leaf2.pem"), &files.cert).unwrap();
    std::fs::copy(fixture("leaf2.key"), &files.key).unwrap();

    let client = builder_with(&load_ca_file(&fixture("ca2.pem")).unwrap())
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let url = format!("https://localhost:{}/", addr.port());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        match client.get(&url).send().await {
            Ok(resp) if resp.status() == 200 => return,
            _ if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await
            }
            other => panic!("the rotated certificate was never served: {other:?}"),
        }
    }
}

/// The listener offers ALPN `h2`, so it must speak HTTP/2 when a client picks
/// it (curl, browsers, Go clients do). Raw check: send the HTTP/2 preface and an
/// empty SETTINGS frame; an HTTP/2 server answers with its own SETTINGS frame
/// (type 0x4). Run with `-p wayhouse-http` alone, where no other crate's features can
/// switch HTTP/2 on behind this crate's back.
#[tokio::test]
async fn speaks_http2_when_alpn_picks_it() {
    use tokio_rustls::rustls::pki_types::pem::PemObject;
    use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};
    use tokio_rustls::rustls::{crypto::ring, ClientConfig, RootCertStore};

    let (addr, _) = serve(fixture_files()).await;
    let mut roots = RootCertStore::empty();
    for c in CertificateDer::pem_file_iter(fixture("ca.pem")).unwrap() {
        roots.add(c.unwrap()).unwrap();
    }
    let mut config = ClientConfig::builder_with_provider(Arc::new(ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"h2".to_vec()];
    let tcp = TcpStream::connect(addr).await.unwrap();
    let mut tls = tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(ServerName::try_from("localhost").unwrap(), tcp)
        .await
        .unwrap();
    assert_eq!(tls.get_ref().1.alpn_protocol(), Some(&b"h2"[..]));
    tls.write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n")
        .await
        .unwrap();
    tls.write_all(&[0, 0, 0, 0x4, 0, 0, 0, 0, 0]).await.unwrap(); // empty SETTINGS
    let mut header = [0u8; 9];
    tokio::time::timeout(Duration::from_secs(2), tls.read_exact(&mut header))
        .await
        .expect("no HTTP/2 answer within 2 s")
        .expect("the server closed the connection instead of speaking HTTP/2");
    assert_eq!(header[3], 0x4, "first frame is not SETTINGS: {header:?}");
}

/// A connection from `src` (any 127.0.0.0/8 address works on Linux loopback) that
/// never says hello.
async fn idle_from(src: &str, addr: SocketAddr) -> TcpStream {
    let socket = TcpSocket::new_v4().unwrap();
    socket.bind(format!("{src}:0").parse().unwrap()).unwrap();
    socket.connect(addr).await.unwrap()
}

/// Whether the server closed `conn` (EOF or reset) within `within`.
async fn closed_within(conn: &mut TcpStream, within: Duration) -> bool {
    let mut buf = [0u8; 1];
    matches!(
        tokio::time::timeout(within, conn.read(&mut buf)).await,
        Ok(Ok(0)) | Ok(Err(_))
    )
}

fn limits(max_pending: usize, max_pending_per_source: usize) -> HandshakeLimits {
    HandshakeLimits {
        max_pending,
        max_pending_per_source,
        ..HandshakeLimits::default()
    }
}

#[tokio::test]
async fn a_source_over_its_cap_is_closed_while_others_get_in() {
    let (addr, _) = serve_with(fixture_files(), limits(64, 2)).await;
    let mut first = idle_from("127.0.0.3", addr).await;
    let mut second = idle_from("127.0.0.3", addr).await;
    let mut third = idle_from("127.0.0.3", addr).await;
    assert!(
        closed_within(&mut third, Duration::from_secs(2)).await,
        "a third pending handshake from one source was kept"
    );
    assert!(!closed_within(&mut first, Duration::from_millis(200)).await);
    assert!(!closed_within(&mut second, Duration::from_millis(200)).await);
    // 127.0.0.1 is another source: the real client gets in.
    get_ok("ca.pem", addr).await;
}

#[tokio::test]
async fn at_the_global_cap_the_oldest_pending_handshake_is_dropped() {
    let (addr, _) = serve_with(fixture_files(), limits(2, 16)).await;
    let mut oldest = idle_from("127.0.0.4", addr).await;
    // Let the server admit it before the next one, so "oldest" is well defined.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut middle = idle_from("127.0.0.5", addr).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut newest = idle_from("127.0.0.6", addr).await;
    assert!(
        closed_within(&mut oldest, Duration::from_secs(2)).await,
        "the oldest pending handshake was kept past the global cap"
    );
    assert!(!closed_within(&mut newest, Duration::from_millis(200)).await);
    // A real client is never refused at the cap: it evicts `middle`.
    tokio::time::timeout(Duration::from_secs(3), get_ok("ca.pem", addr))
        .await
        .expect("a real client was locked out at the global cap");
    assert!(closed_within(&mut middle, Duration::from_secs(2)).await);
}

#[tokio::test]
async fn a_silent_client_is_dropped_at_the_client_hello_deadline() {
    let limits = HandshakeLimits {
        client_hello_timeout: Duration::from_millis(300),
        ..HandshakeLimits::default()
    };
    let (addr, _) = serve_with(fixture_files(), limits).await;
    let mut silent = idle_from("127.0.0.7", addr).await;
    assert!(
        closed_within(&mut silent, Duration::from_secs(3)).await,
        "a client that never sent a ClientHello was kept past the deadline"
    );
    get_ok("ca.pem", addr).await;
}

#[tokio::test]
async fn finished_handshakes_free_their_slot() {
    // One pending handshake per source: three clients in a row from 127.0.0.1
    // each need the previous one's slot back.
    let (addr, _) = serve_with(fixture_files(), limits(64, 1)).await;
    for _ in 0..3 {
        get_ok("ca.pem", addr).await;
    }
}

#[test]
fn the_default_limits() {
    let d = HandshakeLimits::default();
    assert_eq!(d.client_hello_timeout, Duration::from_secs(3));
    assert_eq!(d.handshake_timeout, wayhouse_http::tls::HANDSHAKE_TIMEOUT);
    assert_eq!(d.max_pending, 512);
    assert_eq!(d.max_pending_per_source, 16);
    assert_eq!(d.new_per_source_per_sec, 20.0);
    assert_eq!(d.new_per_source_burst, 64);
}

#[tokio::test]
async fn a_source_over_its_connect_rate_is_closed_while_others_get_in() {
    let limits = HandshakeLimits {
        new_per_source_per_sec: 0.1,
        new_per_source_burst: 2,
        ..HandshakeLimits::default()
    };
    let (addr, _) = serve_with(fixture_files(), limits).await;
    let mut first = idle_from("127.0.0.9", addr).await;
    let mut second = idle_from("127.0.0.9", addr).await;
    let mut third = idle_from("127.0.0.9", addr).await;
    assert!(
        closed_within(&mut third, Duration::from_secs(2)).await,
        "a connect past the source's burst was kept"
    );
    assert!(!closed_within(&mut first, Duration::from_millis(200)).await);
    assert!(!closed_within(&mut second, Duration::from_millis(200)).await);
    // 127.0.0.1 is another source with its own bucket.
    get_ok("ca.pem", addr).await;
}
