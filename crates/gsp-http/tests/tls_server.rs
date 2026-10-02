//! Native TLS: `TlsListener` under a real `axum::serve`, loopback only.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::routing::get;
use axum::Router;
use gsp_http::tls::{spawn_reloader, ReloadingCert, TlsFiles, TlsListener};
use gsp_http::{builder_with, load_ca_file};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

async fn serve(files: TlsFiles) -> (SocketAddr, Arc<ReloadingCert>) {
    let cert = ReloadingCert::new(files).unwrap();
    let listener = TlsListener::bind("127.0.0.1:0".parse().unwrap(), cert.clone())
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
    let chain = gsp_http::error_chain(&err);
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

/// Handshakes in flight are capped; a client stalled before its hello holds a
/// slot until it goes away (or the handshake timeout ends it).
#[tokio::test]
async fn the_handshake_cap_holds_new_clients_until_a_slot_frees() {
    let cert = ReloadingCert::new(fixture_files()).unwrap();
    let listener = TlsListener::bind_with_handshake_limit("127.0.0.1:0".parse().unwrap(), cert, 1)
        .await
        .unwrap();
    let addr = axum::serve::Listener::local_addr(&listener).unwrap();
    let app = Router::new().route("/", get(|| async { "ok" }));
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let stalled = TcpStream::connect(addr).await.unwrap(); // takes the only slot
    tokio::time::sleep(Duration::from_millis(100)).await;
    let waiting = tokio::spawn(get_ok("ca.pem", addr));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!waiting.is_finished(), "a handshake ran past the cap");
    drop(stalled);
    tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .expect("the waiting client never got the freed slot")
        .unwrap();
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
    // No wait for the mtime to move on: a rewrite changes the ctime regardless.
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
/// (type 0x4). Run with `-p gsp-http` alone, where no other crate's features can
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
