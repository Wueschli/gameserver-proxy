//! Native TLS for the fleet's HTTP servers (`--tls-cert/--tls-key`, [`TlsArgs`]).
//!
//! [`ReloadingCert`] holds the served certificate behind an `ArcSwap` and re-reads
//! the PEM files when they change, so a renewed certificate (certbot,
//! cert-manager) is picked up without a restart; a broken or half-written
//! replacement keeps the current one. [`TlsListener`] is an `axum::serve::Listener`
//! that runs each handshake in its own task, so a slow client never blocks accepts.
//! [`serve`] picks HTTPS or plain HTTP for a binary's `--listen`.

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

/// The certificate (PEM chain, leaf first) and private key (PEM) files, plus the
/// names errors call them by: `--tls-cert`/`--tls-key` unless [`TlsFiles::named`].
#[derive(Clone, Debug)]
pub struct TlsFiles {
    pub cert: PathBuf,
    pub key: PathBuf,
    pub cert_name: &'static str,
    pub key_name: &'static str,
}

impl TlsFiles {
    pub fn new(cert: PathBuf, key: PathBuf) -> Self {
        Self {
            cert,
            key,
            cert_name: "--tls-cert",
            key_name: "--tls-key",
        }
    }

    /// Name the pair after where it was configured (e.g. a YAML setting).
    pub fn named(mut self, cert_name: &'static str, key_name: &'static str) -> Self {
        self.cert_name = cert_name;
        self.key_name = key_name;
        self
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("{name} {}: cannot read the file", path.display())]
    Read {
        name: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    #[error("{name} {}: no PEM certificate found", path.display())]
    NoCertificate { name: &'static str, path: PathBuf },
    #[error("{name} {}: malformed PEM certificate", path.display())]
    BadCertificate {
        name: &'static str,
        path: PathBuf,
        source: tokio_rustls::rustls::pki_types::pem::Error,
    },
    #[error("{name} {}: no PEM private key found", path.display())]
    NoKey { name: &'static str, path: PathBuf },
    #[error("{name} {}: unsupported or invalid private key", path.display())]
    BadKey {
        name: &'static str,
        path: PathBuf,
        source: rustls::Error,
    },
    #[error("--tls-cert and --tls-key go together; only one was given")]
    Incomplete,
    #[error(
        "{} {} does not match the certificate in {} {}",
        files.key_name,
        files.key.display(),
        files.cert_name,
        files.cert.display()
    )]
    KeyMismatch {
        files: Box<TlsFiles>,
        source: rustls::Error,
    },
}

fn read(name: &'static str, path: &Path) -> Result<Vec<u8>, TlsError> {
    std::fs::read(path).map_err(|source| TlsError::Read {
        name,
        path: path.to_owned(),
        source,
    })
}

/// Read and validate the pair: at least one certificate, a key rustls (ring)
/// supports in any PEM encoding, and a key that matches the leaf certificate.
pub fn load_certified_key(files: &TlsFiles) -> Result<CertifiedKey, TlsError> {
    let cert_pem = read(files.cert_name, &files.cert)?;
    // Every PEM section must parse: a file cut off mid-chain is an error, not a
    // shorter chain.
    let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(&cert_pem)
        .collect::<Result<_, _>>()
        .map_err(|source| TlsError::BadCertificate {
            name: files.cert_name,
            path: files.cert.clone(),
            source,
        })?;
    if chain.is_empty() {
        return Err(TlsError::NoCertificate {
            name: files.cert_name,
            path: files.cert.clone(),
        });
    }
    let key_pem = read(files.key_name, &files.key)?;
    let key = PrivateKeyDer::from_pem_slice(&key_pem).map_err(|_| TlsError::NoKey {
        name: files.key_name,
        path: files.key.clone(),
    })?;
    let signer = rustls::crypto::ring::sign::any_supported_type(&key).map_err(|source| {
        TlsError::BadKey {
            name: files.key_name,
            path: files.key.clone(),
            source,
        }
    })?;
    let certified = CertifiedKey::new(chain, signer);
    certified
        .keys_match()
        .map_err(|source| TlsError::KeyMismatch {
            files: Box::new(files.clone()),
            source,
        })?;
    Ok(certified)
}

/// What "this file changed" is judged by: mtime, size and — on Unix — inode and
/// ctime. ctime cannot be set from user space, so a rewrite that restores the
/// mtime (`cp -p`, `touch -r`) still shows. On a coarse-timestamp filesystem
/// ctime is as coarse as mtime; there a same-length, same-inode rewrite within
/// one tick of the last load would be missed until the next change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileStamp {
    mtime: Option<SystemTime>,
    len: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(unix)]
    ctime: (i64, i64),
}

fn stamp(path: &Path) -> Option<FileStamp> {
    let m = std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt as _;
    Some(FileStamp {
        mtime: m.modified().ok(),
        len: m.len(),
        #[cfg(unix)]
        ino: m.ino(),
        #[cfg(unix)]
        ctime: (m.ctime(), m.ctime_nsec()),
    })
}

type PairStamp = (Option<FileStamp>, Option<FileStamp>);

fn stamps(files: &TlsFiles) -> PairStamp {
    (stamp(&files.cert), stamp(&files.key))
}

/// The served certificate, swappable at runtime (see the module doc).
pub struct ReloadingCert {
    files: TlsFiles,
    current: ArcSwap<CertifiedKey>,
    /// Stamps of the pair last loaded successfully; a failed reload leaves them,
    /// so the next poll tries again.
    loaded: Mutex<PairStamp>,
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
        let stamp = stamps(&files);
        let key = load_certified_key(&files)?;
        Ok(Arc::new(Self {
            files,
            current: ArcSwap::from_pointee(key),
            loaded: Mutex::new(stamp),
        }))
    }

    /// Re-read the pair if either file changed (see [`FileStamp`]) since the last
    /// good load. `Ok(true)` = swapped; on `Err` the current certificate stays.
    pub fn reload_if_changed(&self) -> Result<bool, TlsError> {
        let stamp = stamps(&self.files);
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
    /// No cap on handshakes in flight, on purpose: a global cap lets ~cap idle
    /// connects lock every client out. A stalled client costs one task and one
    /// fd until [`HANDSHAKE_TIMEOUT`], the same bound as plain `axum::serve`.
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
            // The accept task loops forever and is only aborted with this
            // listener, so this means it panicked: say so, then serve no more.
            None => {
                tracing::error!("the TLS accept task ended; no new connections will be accepted");
                std::future::pending().await
            }
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

// `--tls-cert`/`--tls-key`, shared by every fleet binary that serves HTTP
// (`#[command(flatten)]` into its `Args`). A `//` comment on purpose: clap turns a
// `///` doc on a flattened struct into the parent command's `about`.
#[derive(clap::Args, Clone, Debug, Default)]
pub struct TlsArgs {
    /// PEM certificate chain (leaf first) to serve HTTPS with on `--listen`
    /// instead of plain HTTP. Requires `--tls-key`. Re-read when the file
    /// changes (checked every 30 s), so a renewed certificate needs no restart.
    #[arg(long, requires = "tls_key")]
    pub tls_cert: Option<PathBuf>,

    /// PEM private key for `--tls-cert`.
    #[arg(long, requires = "tls_cert")]
    pub tls_key: Option<PathBuf>,
}

impl TlsArgs {
    /// Load and validate the pair, if given (a bad file is a startup error).
    /// Only one of the two is an error, never a silent fall-back to plain HTTP.
    pub fn load(&self) -> Result<Option<Arc<ReloadingCert>>, TlsError> {
        match (&self.tls_cert, &self.tls_key) {
            (Some(cert), Some(key)) => {
                ReloadingCert::new(TlsFiles::new(cert.clone(), key.clone())).map(Some)
            }
            (None, None) => Ok(None),
            _ => Err(TlsError::Incomplete),
        }
    }
}

/// How often a served certificate's files are checked for a renewed pair.
pub const RELOAD_EVERY: Duration = Duration::from_secs(30);

/// Serve `app` on `addr` until the server fails: HTTPS through [`TlsListener`]
/// (with the files re-read every [`RELOAD_EVERY`]) when `cert` is set, plain
/// HTTP otherwise. `name` is the binary, for the startup log line.
pub async fn serve(
    addr: SocketAddr,
    app: axum::Router,
    cert: Option<Arc<ReloadingCert>>,
    name: &str,
) -> io::Result<()> {
    match cert {
        Some(cert) => {
            let listener = TlsListener::bind(addr, cert.clone()).await?;
            let _reloader = AbortOnDrop(spawn_reloader(cert, RELOAD_EVERY));
            tracing::info!(listen = %addr, "{name} serving HTTPS");
            axum::serve(listener, app).await
        }
        None => {
            let listener = TcpListener::bind(addr).await?;
            tracing::info!(listen = %addr, "{name} listening");
            axum::serve(listener, app).await
        }
    }
}
