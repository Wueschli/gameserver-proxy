//! The TLS terminator's connection counter counts completed TLS handshakes,
//! not TCP accepts: `ha_tls` reads it as "TLS connections opened".

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::rustls::crypto::ring;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;
use wayhouse_fleet_tests::tls_front::{tls_front_counted, TEST_CA};

#[tokio::test]
async fn the_counter_counts_tls_handshakes_not_tcp_accepts() -> Result<()> {
    let upstream = TcpListener::bind("127.0.0.1:0").await?;
    let (front, _task, count) = tls_front_counted(upstream.local_addr()?).await?;

    // A TCP connection that never speaks TLS is not a TLS connection.
    drop(TcpStream::connect(front).await?);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        count.load(Ordering::Relaxed),
        0,
        "a bare TCP connect counted"
    );

    let mut roots = RootCertStore::empty();
    for ca in CertificateDer::pem_file_iter(TEST_CA)? {
        roots.add(ca?)?;
    }
    let client = ClientConfig::builder_with_provider(Arc::new(ring::default_provider()))
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let tls = TlsConnector::from(Arc::new(client))
        .connect(
            ServerName::try_from("localhost")?,
            TcpStream::connect(front).await?,
        )
        .await?;
    // TLS 1.3 finishes on the client first; the terminator counts a moment later.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(count.load(Ordering::Relaxed), 1, "a completed handshake");
    drop(tls);
    Ok(())
}
