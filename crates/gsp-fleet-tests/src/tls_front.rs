//! A TLS terminator for `--ca-file` tests: accepts TLS with the test-only
//! `localhost` certificate from `crates/gsp-http/tests/fixtures/` (signed by a
//! private CA no client trusts by default) and pipes the plaintext to an
//! upstream TCP address — the docs/12 "gsp-controller behind TLS" shape.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_rustls::rustls::crypto::ring;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;

/// Path of the private test CA that signed the terminator's certificate.
pub const TEST_CA: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../gsp-http/tests/fixtures/ca.pem"
);

/// Paths of the test-only `localhost` certificate and key the CA above signed,
/// for a binary that serves TLS itself (`--tls-cert`/`--tls-key`).
pub const TEST_LEAF: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../gsp-http/tests/fixtures/leaf.pem"
);
pub const TEST_LEAF_KEY: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../gsp-http/tests/fixtures/leaf.key"
);

const LEAF: &[u8] = include_bytes!("../../gsp-http/tests/fixtures/leaf.pem");
const LEAF_KEY: &[u8] = include_bytes!("../../gsp-http/tests/fixtures/leaf.key");

/// Listen on an ephemeral loopback port; returns it and the accept task
/// (dropping the handle does not stop it; the test process exit does).
pub async fn tls_front(upstream: SocketAddr) -> Result<(SocketAddr, JoinHandle<()>)> {
    let (addr, task, _) = tls_front_counted(upstream).await?;
    Ok((addr, task))
}

/// [`tls_front`], plus a count of TCP connections accepted so far — how a
/// test tells a client that reuses its connections from one that dials anew
/// for every request.
pub async fn tls_front_counted(
    upstream: SocketAddr,
) -> Result<(SocketAddr, JoinHandle<()>, Arc<AtomicUsize>)> {
    let certs = CertificateDer::pem_slice_iter(LEAF)
        .collect::<Result<Vec<_>, _>>()
        .context("parsing the fixture certificate")?;
    let key = PrivateKeyDer::from_pem_slice(LEAF_KEY).context("parsing the fixture key")?;
    let config = ServerConfig::builder_with_provider(Arc::new(ring::default_provider()))
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let accepted = Arc::new(AtomicUsize::new(0));
    let counter = accepted.clone();
    let task = tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            counter.fetch_add(1, Ordering::Relaxed);
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let Ok(mut up) = TcpStream::connect(upstream).await else {
                    return;
                };
                let _ = tokio::io::copy_bidirectional(&mut tls, &mut up).await;
            });
        }
    });
    Ok((addr, task, accepted))
}
