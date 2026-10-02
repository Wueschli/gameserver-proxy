//! Native TLS for the fleet's HTTP servers (`gsp-controller --tls-cert/--tls-key`).
//!
//! [`ReloadingCert`] holds the served certificate behind an `ArcSwap` and re-reads
//! the PEM files when their mtime changes, so a renewed certificate (certbot,
//! cert-manager) is picked up without a restart; a broken or half-written
//! replacement keeps the current one. [`TlsListener`] is an `axum::serve::Listener`
//! that runs each handshake in its own task, so a slow client never blocks accepts.

use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use arc_swap::ArcSwap;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_rustls::rustls;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::server::{ClientHello, ResolvesServerCert};
use tokio_rustls::rustls::sign::CertifiedKey;
use tokio_rustls::server::TlsStream;
use tokio_rustls::TlsAcceptor;

/// The `--tls-cert` (PEM chain, leaf first) and `--tls-key` (PEM private key) files.
#[derive(Clone, Debug)]
pub struct TlsFiles {
    pub cert: PathBuf,
    pub key: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("{flag} {}: cannot read the file", path.display())]
    Read {
        flag: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    #[error("--tls-cert {}: no PEM certificate found", path.display())]
    NoCertificate { path: PathBuf },
    #[error("--tls-key {}: no PEM private key found", path.display())]
    NoKey { path: PathBuf },
    #[error("--tls-key {}: unsupported or invalid private key", path.display())]
    BadKey {
        path: PathBuf,
        source: rustls::Error,
    },
    #[error("--tls-key {} does not match the certificate in --tls-cert {}", key.display(), cert.display())]
    KeyMismatch {
        cert: PathBuf,
        key: PathBuf,
        source: rustls::Error,
    },
}

fn read(flag: &'static str, path: &Path) -> Result<Vec<u8>, TlsError> {
    std::fs::read(path).map_err(|source| TlsError::Read {
        flag,
        path: path.to_owned(),
        source,
    })
}

/// Read and validate the pair: at least one certificate, a key rustls (ring)
/// supports in any PEM encoding, and a key that matches the leaf certificate.
pub fn load_certified_key(files: &TlsFiles) -> Result<CertifiedKey, TlsError> {
    let cert_pem = read("--tls-cert", &files.cert)?;
    let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(&cert_pem)
        .filter_map(Result::ok)
        .collect();
    if chain.is_empty() {
        return Err(TlsError::NoCertificate {
            path: files.cert.clone(),
        });
    }
    let key_pem = read("--tls-key", &files.key)?;
    let key = PrivateKeyDer::from_pem_slice(&key_pem).map_err(|_| TlsError::NoKey {
        path: files.key.clone(),
    })?;
    let signer = rustls::crypto::ring::sign::any_supported_type(&key).map_err(|source| {
        TlsError::BadKey {
            path: files.key.clone(),
            source,
        }
    })?;
    let certified = CertifiedKey::new(chain, signer);
    certified
        .keys_match()
        .map_err(|source| TlsError::KeyMismatch {
            cert: files.cert.clone(),
            key: files.key.clone(),
            source,
        })?;
    Ok(certified)
}

fn mtimes(files: &TlsFiles) -> (Option<SystemTime>, Option<SystemTime>) {
    let m = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    (m(&files.cert), m(&files.key))
}

/// The served certificate, swappable at runtime (see the module doc).
pub struct ReloadingCert {
    files: TlsFiles,
    current: ArcSwap<CertifiedKey>,
    /// mtimes of the pair last loaded successfully; a failed reload leaves them,
    /// so the next poll tries again.
    loaded: Mutex<(Option<SystemTime>, Option<SystemTime>)>,
}

impl fmt::Debug for ReloadingCert {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReloadingCert")
            .field("files", &self.files)
            .finish_non_exhaustive()
    }
}

impl ReloadingCert {
    /// Load the pair (an error here is a startup error).
    pub fn new(files: TlsFiles) -> Result<Arc<Self>, TlsError> {
        let stamp = mtimes(&files);
        let key = load_certified_key(&files)?;
        Ok(Arc::new(Self {
            files,
            current: ArcSwap::from_pointee(key),
            loaded: Mutex::new(stamp),
        }))
    }

    /// Re-read the pair if either file's mtime changed since the last good load.
    /// `Ok(true)` = swapped; on `Err` the current certificate stays in place.
    pub fn reload_if_changed(&self) -> Result<bool, TlsError> {
        let stamp = mtimes(&self.files);
        let mut loaded = self.loaded.lock().expect("reload lock poisoned");
        if *loaded == stamp {
            return Ok(false);
        }
        let key = load_certified_key(&self.files)?;
        self.current.store(Arc::new(key));
        *loaded = stamp;
        Ok(true)
    }

    /// The certificate new handshakes are served with.
    pub fn current(&self) -> Arc<CertifiedKey> {
        self.current.load_full()
    }

    pub fn files(&self) -> &TlsFiles {
        &self.files
    }
}

impl ResolvesServerCert for ReloadingCert {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.current())
    }
}

/// How long a client gets to finish the TLS handshake before it is dropped.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Finished handshakes waiting for `axum::serve` to pick them up.
const READY_BACKLOG: usize = 64;

/// An `axum::serve::Listener` serving TLS with a [`ReloadingCert`]. A background
/// task accepts TCP connections and runs each handshake in its own task, so a
/// client that connects and stalls costs one task, never the accept loop.
pub struct TlsListener {
    ready: mpsc::Receiver<(TlsStream<TcpStream>, SocketAddr)>,
    local: SocketAddr,
    _accept: AbortOnDrop,
}

struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl TlsListener {
    pub async fn bind(addr: SocketAddr, cert: Arc<ReloadingCert>) -> io::Result<Self> {
        let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(io::Error::other)?
        .with_no_client_auth()
        .with_cert_resolver(cert);
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        let acceptor = TlsAcceptor::from(Arc::new(config));

        let tcp = TcpListener::bind(addr).await?;
        let local = tcp.local_addr()?;
        let (tx, ready) = mpsc::channel(READY_BACKLOG);
        let accept = tokio::spawn(async move {
            loop {
                let (stream, peer) = match tcp.accept().await {
                    Ok(conn) => conn,
                    Err(e) => {
                        // Like axum's own listener: e.g. EMFILE — back off, keep serving.
                        tracing::warn!(error = %e, "accepting a TCP connection failed");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                };
                let (acceptor, tx) = (acceptor.clone(), tx.clone());
                tokio::spawn(async move {
                    match tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await {
                        Ok(Ok(tls)) => {
                            let _ = tx.send((tls, peer)).await;
                        }
                        Ok(Err(e)) => tracing::debug!(%peer, error = %e, "TLS handshake failed"),
                        Err(_) => tracing::debug!(%peer, "TLS handshake timed out"),
                    }
                });
            }
        });
        Ok(Self {
            ready,
            local,
            _accept: AbortOnDrop(accept),
        })
    }
}

impl axum::serve::Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.ready.recv().await {
            Some(conn) => conn,
            // The accept task only ends if aborted, i.e. this listener is gone.
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        Ok(self.local)
    }
}

/// Poll `cert`'s files every `every` (30 s in production) and swap in a changed
/// pair; a failed reload is logged and keeps the current certificate.
pub fn spawn_reloader(cert: Arc<ReloadingCert>, every: Duration) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            match cert.reload_if_changed() {
                Ok(true) => tracing::info!(
                    cert = %cert.files().cert.display(),
                    "reloaded the TLS certificate"
                ),
                Ok(false) => {}
                Err(e) => tracing::error!(
                    error = %crate::error_chain(&e),
                    "TLS certificate reload failed; still serving the previous one"
                ),
            }
        }
    })
}
