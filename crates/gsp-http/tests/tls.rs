//! `--ca-file` against a real TLS server whose certificate a private test CA
//! signed (`tests/fixtures/README.md`). Loopback only.

use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gsp_http::{builder_with, client, init_ca_file, load_ca_file, CaError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::{crypto::ring, ServerConfig};
use tokio_rustls::TlsAcceptor;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn read(name: &str) -> Vec<u8> {
    std::fs::read(fixture(name)).unwrap()
}

/// A temp file holding the concatenation of `parts`.
fn bundle(parts: &[&[u8]]) -> tempfile::NamedTempFile {
    let mut f = tempfile::NamedTempFile::new().unwrap();
    for p in parts {
        f.write_all(p).unwrap();
    }
    f
}

/// Serves `HTTP/1.1 200` with body `ok` on every connection, over TLS with the
/// fixture `localhost` certificate.
async fn tls_server() -> SocketAddr {
    let certs = CertificateDer::pem_file_iter(fixture("leaf.pem"))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let key = PrivateKeyDer::from_pem_file(fixture("leaf.key")).unwrap();
    let config = ServerConfig::builder_with_provider(Arc::new(ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let mut buf = [0u8; 4096];
                let _ = tls.read(&mut buf).await;
                let _ = tls
                    .write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
                    )
                    .await;
                let _ = tls.shutdown().await;
            });
        }
    });
    addr
}

fn url(addr: SocketAddr) -> String {
    format!("https://localhost:{}/", addr.port())
}

async fn get_ok(client: &reqwest::Client, addr: SocketAddr) {
    let resp = client.get(url(addr)).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "ok");
}

#[tokio::test]
async fn plain_client_rejects_the_private_ca() {
    let addr = tls_server().await;
    let err = builder_with(&[])
        .build()
        .unwrap()
        .get(url(addr))
        .send()
        .await
        .expect_err("the fixture CA must not be publicly trusted");
    assert!(err.is_connect(), "{err:?}");
}

#[tokio::test]
async fn trusts_the_ca_from_a_file() {
    let addr = tls_server().await;
    let certs = load_ca_file(&fixture("ca.pem")).unwrap();
    assert_eq!(certs.len(), 1);
    get_ok(&builder_with(&certs).build().unwrap(), addr).await;
}

#[tokio::test]
async fn trusts_every_cert_in_a_bundle() {
    let addr = tls_server().await;
    let f = bundle(&[&read("other-ca.pem"), &read("ca.pem")]);
    let certs = load_ca_file(f.path()).unwrap();
    assert_eq!(certs.len(), 2);
    get_ok(&builder_with(&certs).build().unwrap(), addr).await;
}

#[tokio::test]
async fn ignores_non_certificate_pem_sections() {
    let addr = tls_server().await;
    let f = bundle(&[&read("leaf.key"), &read("ca.pem")]);
    let certs = load_ca_file(f.path()).unwrap();
    assert_eq!(certs.len(), 1);
    get_ok(&builder_with(&certs).build().unwrap(), addr).await;
}

#[test]
fn missing_file_is_a_read_error() {
    let path = Path::new("/nonexistent/gsp-ca.pem");
    let e = load_ca_file(path).unwrap_err();
    assert!(matches!(e, CaError::Read { .. }), "{e:?}");
    assert!(
        e.to_string()
            .starts_with("--ca-file /nonexistent/gsp-ca.pem: "),
        "{e}"
    );
}

#[test]
fn file_without_certificates_is_rejected() {
    let f = bundle(&[&read("leaf.key")]);
    let e = load_ca_file(f.path()).unwrap_err();
    assert!(matches!(e, CaError::NoCertificates { .. }), "{e:?}");
    assert!(
        e.to_string().contains(&f.path().display().to_string()),
        "{e}"
    );
}

#[test]
fn rejects_a_malformed_certificate() {
    let f = bundle(&[b"-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n"]);
    let e = load_ca_file(f.path()).unwrap_err();
    assert!(matches!(e, CaError::Parse { .. }), "{e:?}");
}

/// The only test that touches the process-global roots.
#[tokio::test]
async fn init_twice_is_an_error() {
    let addr = tls_server().await;
    assert_eq!(init_ca_file(&fixture("ca.pem")).unwrap(), 1);
    assert!(matches!(
        init_ca_file(&fixture("ca.pem")),
        Err(CaError::AlreadyInitialised)
    ));
    get_ok(&client(), addr).await;
}
