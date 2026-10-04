//! Native TLS for the fleet's HTTP servers (`--tls-cert/--tls-key`, [`TlsArgs`]).
//!
//! [`ReloadingCert`] holds the served certificate behind an `ArcSwap` and re-reads
//! the PEM files when they change, so a renewed certificate (certbot,
//! cert-manager) is picked up without a restart; a broken or half-written
//! replacement keeps the current one. [`TlsListener`] is an `axum::serve::Listener`
//! that runs each handshake in its own task, so a slow client never blocks accepts,
//! and bounds the handshakes in flight per source and in total ([`HandshakeLimits`]).
//! [`serve`] picks HTTPS or plain HTTP for a binary's `--listen`.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use arc_swap::ArcSwap;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::{AbortHandle, JoinHandle};
use tokio_rustls::rustls;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::server::{ClientHello, ResolvesServerCert};
use tokio_rustls::rustls::sign::CertifiedKey;
use tokio_rustls::server::TlsStream;
use tokio_rustls::LazyConfigAcceptor;

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
        let mut loaded = self.loaded.lock().unwrap_or_else(PoisonError::into_inner);
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

/// How long a client gets to send its ClientHello (part of [`HANDSHAKE_TIMEOUT`]).
pub const CLIENT_HELLO_TIMEOUT: Duration = Duration::from_secs(3);

/// Finished handshakes waiting for `axum::serve` to pick them up.
const READY_BACKLOG: usize = 64;

/// At most one "handshake limit reached" warning per this long.
const LIMIT_WARN_EVERY: Duration = Duration::from_secs(60);

/// Bounds on handshakes in flight (accepted, not yet finished), so a flood of
/// connects that never finish cannot use up the process's fds.
///
/// Over `max_pending_per_source` (a source is an IPv4 address or an IPv6 /64) a
/// new connection is closed at once. At `max_pending` a new connection is still
/// admitted and the *oldest* pending handshake is dropped instead: refusing at a
/// global cap would let ~cap idle connects lock every client out, while evicting
/// only hurts a real client if the cap's worth of connects arrive within its one
/// RTT of handshake.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HandshakeLimits {
    pub client_hello_timeout: Duration,
    pub handshake_timeout: Duration,
    pub max_pending: usize,
    pub max_pending_per_source: usize,
}

impl Default for HandshakeLimits {
    fn default() -> Self {
        Self {
            client_hello_timeout: CLIENT_HELLO_TIMEOUT,
            handshake_timeout: HANDSHAKE_TIMEOUT,
            max_pending: 512,
            max_pending_per_source: 16,
        }
    }
}

/// What the per-source cap counts by: an IPv4 address, or an IPv6 /64 (anyone
/// with IPv6 has a whole /64). An IPv4-mapped IPv6 peer is its IPv4 address.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum SourceKey {
    V4(Ipv4Addr),
    V6(u64),
}

fn source_key(ip: IpAddr) -> SourceKey {
    match ip {
        IpAddr::V4(v4) => SourceKey::V4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => SourceKey::V4(v4),
            None => SourceKey::V6((u128::from(v6) >> 64) as u64),
        },
    }
}

/// The pending-handshake bookkeeping (see [`HandshakeLimits`]): ids in admission
/// order, so the oldest is the first.
struct Pending {
    max_pending: usize,
    max_pending_per_source: usize,
    order: BTreeMap<u64, SourceKey>,
    per_source: HashMap<SourceKey, usize>,
    next_id: u64,
}

impl Pending {
    fn new(limits: &HandshakeLimits) -> Self {
        Self {
            // A cap of 0 would evict every handshake as it is admitted.
            max_pending: limits.max_pending.max(1),
            max_pending_per_source: limits.max_pending_per_source.max(1),
            order: BTreeMap::new(),
            per_source: HashMap::new(),
            next_id: 0,
        }
    }

    /// Admit a handshake from `key`: its id and the id of the handshake evicted
    /// to make room, or `None` when `key` is at its cap.
    fn admit(&mut self, key: SourceKey) -> Option<(u64, Option<u64>)> {
        if self.per_source.get(&key).copied().unwrap_or(0) >= self.max_pending_per_source {
            return None;
        }
        let evicted = if self.order.len() >= self.max_pending {
            self.order.first_key_value().map(|(&id, _)| id)
        } else {
            None
        };
        if let Some(id) = evicted {
            self.release(id);
        }
        let id = self.next_id;
        self.next_id += 1;
        self.order.insert(id, key);
        *self.per_source.entry(key).or_insert(0) += 1;
        Some((id, evicted))
    }

    /// Free `id`'s slot; a no-op for an id already released or evicted.
    fn release(&mut self, id: u64) {
        let Some(key) = self.order.remove(&id) else {
            return;
        };
        if let Some(n) = self.per_source.get_mut(&key) {
            *n -= 1;
            if *n == 0 {
                self.per_source.remove(&key);
            }
        }
    }

    fn contains(&self, id: u64) -> bool {
        self.order.contains_key(&id)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.order.len()
    }
}

/// [`Pending`] plus what eviction needs: each pending handshake's task.
struct Handshakes {
    pending: Pending,
    tasks: HashMap<u64, AbortHandle>,
    last_warn: Option<Instant>,
}

impl Handshakes {
    /// Remember `id`'s task for eviction, unless it already ended (or was
    /// evicted) between its spawn and now.
    fn register(&mut self, id: u64, task: AbortHandle) {
        if self.pending.contains(id) {
            self.tasks.insert(id, task);
        }
    }

    fn release(&mut self, id: u64) {
        self.pending.release(id);
        self.tasks.remove(&id);
    }

    fn warn_throttled(&mut self, what: &str) {
        let now = Instant::now();
        if self
            .last_warn
            .is_none_or(|t| now.duration_since(t) >= LIMIT_WARN_EVERY)
        {
            self.last_warn = Some(now);
            tracing::warn!(
                "TLS handshake limit reached ({what}); a client may be flooding this \
                 port (repeats are logged at debug for a minute)"
            );
        }
    }
}

/// Frees a handshake's slot when its task ends, however it ends (finished,
/// failed, timed out, or aborted by an eviction).
struct Slot {
    id: u64,
    handshakes: Arc<Mutex<Handshakes>>,
}

impl Drop for Slot {
    fn drop(&mut self) {
        if let Ok(mut h) = self.handshakes.lock() {
            h.release(self.id);
        }
    }
}

/// An `axum::serve::Listener` serving TLS with a [`ReloadingCert`]. A background
/// task accepts TCP connections and runs each handshake in its own task, so a
/// client that connects and stalls costs one task, never the accept loop; the
/// handshakes in flight are bounded by [`HandshakeLimits`].
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
    /// [`bind_with`](Self::bind_with) the default [`HandshakeLimits`].
    pub async fn bind(addr: SocketAddr, cert: Arc<ReloadingCert>) -> io::Result<Self> {
        Self::bind_with(addr, cert, HandshakeLimits::default()).await
    }

    pub async fn bind_with(
        addr: SocketAddr,
        cert: Arc<ReloadingCert>,
        limits: HandshakeLimits,
    ) -> io::Result<Self> {
        let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(io::Error::other)?
        .with_no_client_auth()
        .with_cert_resolver(cert);
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        let config = Arc::new(config);

        let tcp = TcpListener::bind(addr).await?;
        let local = tcp.local_addr()?;
        let (tx, ready) = mpsc::channel(READY_BACKLOG);
        let handshakes = Arc::new(Mutex::new(Handshakes {
            pending: Pending::new(&limits),
            tasks: HashMap::new(),
            last_warn: None,
        }));
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
                // The lock is never held across a spawn, an abort or an `.await`:
                // either can drop a task's future (and its `Slot`) inline.
                let admitted = {
                    let mut h = handshakes.lock().unwrap_or_else(PoisonError::into_inner);
                    match h.pending.admit(source_key(peer.ip())) {
                        None => {
                            h.warn_throttled("per source");
                            None
                        }
                        Some((id, evicted)) => {
                            let evicted = evicted.and_then(|old| h.tasks.remove(&old));
                            if evicted.is_some() {
                                h.warn_throttled("global, dropping the oldest");
                            }
                            Some((id, evicted))
                        }
                    }
                };
                let Some((id, evicted)) = admitted else {
                    tracing::debug!(%peer, "too many TLS handshakes from this source; closed");
                    continue; // drops `stream`
                };
                if let Some(task) = evicted {
                    tracing::debug!(%peer, "TLS handshakes at the cap; dropped the oldest");
                    task.abort();
                }
                let slot = Slot {
                    id,
                    handshakes: handshakes.clone(),
                };
                let (config, tx) = (config.clone(), tx.clone());
                let task = tokio::spawn(async move {
                    let result = handshake(stream, config, &limits).await;
                    drop(slot); // no longer pending, even while `ready` is full
                    match result {
                        Ok(tls) => {
                            let _ = tx.send((tls, peer)).await;
                        }
                        Err(e) => tracing::debug!(%peer, error = %e, "TLS handshake failed"),
                    }
                });
                handshakes
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .register(id, task.abort_handle());
            }
        });
        Ok(Self {
            ready,
            local,
            _accept: AbortOnDrop(accept),
        })
    }
}

/// The ClientHello within `client_hello_timeout`, the whole handshake within
/// `handshake_timeout`.
async fn handshake(
    stream: TcpStream,
    config: Arc<rustls::ServerConfig>,
    limits: &HandshakeLimits,
) -> io::Result<TlsStream<TcpStream>> {
    let deadline = tokio::time::Instant::now() + limits.handshake_timeout;
    let hello = LazyConfigAcceptor::new(rustls::server::Acceptor::default(), stream);
    let start = tokio::time::timeout(
        limits.client_hello_timeout.min(limits.handshake_timeout),
        hello,
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "no ClientHello in time"))??;
    tokio::time::timeout_at(deadline, start.into_stream(config))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "handshake timed out"))?
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

/// The peer's socket address, as a handler sees it through
/// `axum::extract::ConnectInfo<PeerAddr>` (a newtype because axum only
/// implements `Connected` for plain `SocketAddr` over a `TcpListener`).
#[derive(Debug, Clone, Copy)]
pub struct PeerAddr(pub SocketAddr);

impl axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, TlsListener>>
    for PeerAddr
{
    fn connect_info(stream: axum::serve::IncomingStream<'_, TlsListener>) -> Self {
        PeerAddr(*stream.remote_addr())
    }
}

impl axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, TcpListener>>
    for PeerAddr
{
    fn connect_info(stream: axum::serve::IncomingStream<'_, TcpListener>) -> Self {
        PeerAddr(*stream.remote_addr())
    }
}

/// Serve `app` on `addr` until the server fails: HTTPS through [`TlsListener`]
/// (with the files re-read every [`RELOAD_EVERY`]) when `cert` is set, plain
/// HTTP otherwise. `name` is the binary, for the startup log line. Handlers can
/// read the peer address through `axum::extract::ConnectInfo<PeerAddr>`.
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
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<PeerAddr>(),
            )
            .await
        }
        None => {
            let listener = TcpListener::bind(addr).await?;
            tracing::info!(listen = %addr, "{name} listening");
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<PeerAddr>(),
            )
            .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn a_source_is_an_ipv4_address_or_an_ipv6_slash_64() {
        assert_eq!(source_key(ip("192.0.2.1")), source_key(ip("192.0.2.1")));
        assert_ne!(source_key(ip("192.0.2.1")), source_key(ip("192.0.2.2")));
        assert_eq!(
            source_key(ip("2001:db8:1:2::1")),
            source_key(ip("2001:db8:1:2:ffff::9"))
        );
        assert_ne!(
            source_key(ip("2001:db8:1:2::1")),
            source_key(ip("2001:db8:1:3::1"))
        );
        // An IPv4-mapped peer (dual-stack listener) is its IPv4 address, not a /64
        // shared by the whole IPv4 internet.
        assert_eq!(
            source_key(ip("::ffff:192.0.2.1")),
            source_key(ip("192.0.2.1"))
        );
        assert_ne!(
            source_key(ip("::ffff:192.0.2.1")),
            source_key(ip("::ffff:192.0.2.2"))
        );
    }

    fn pending(max_pending: usize, max_pending_per_source: usize) -> Pending {
        Pending::new(&HandshakeLimits {
            max_pending,
            max_pending_per_source,
            ..HandshakeLimits::default()
        })
    }

    #[test]
    fn a_source_at_its_cap_is_refused_until_one_is_released() {
        let mut p = pending(64, 2);
        let a = source_key(ip("192.0.2.1"));
        let b = source_key(ip("192.0.2.2"));
        let (first, _) = p.admit(a).unwrap();
        p.admit(a).unwrap();
        assert!(p.admit(a).is_none());
        p.admit(b)
            .expect("another source is not limited by this one");
        p.release(first);
        p.admit(a).expect("a released slot is free again");
    }

    #[test]
    fn at_the_global_cap_the_oldest_is_evicted() {
        let mut p = pending(2, 16);
        let a = source_key(ip("192.0.2.1"));
        let (oldest, evicted) = p.admit(a).unwrap();
        assert!(evicted.is_none());
        let (middle, evicted) = p.admit(a).unwrap();
        assert!(evicted.is_none());
        let (_, evicted) = p.admit(a).unwrap();
        assert_eq!(evicted, Some(oldest));
        // The evicted handshake's own release later is a no-op …
        p.release(oldest);
        assert_eq!(p.len(), 2);
        // … and the next eviction takes the next oldest.
        let (_, evicted) = p.admit(a).unwrap();
        assert_eq!(evicted, Some(middle));
    }

    #[test]
    fn eviction_frees_the_evicted_source_slot() {
        let mut p = pending(1, 1);
        let a = source_key(ip("192.0.2.1"));
        let b = source_key(ip("192.0.2.2"));
        p.admit(a).unwrap();
        p.admit(b).unwrap(); // evicts a's
        p.admit(a)
            .expect("a's evicted handshake no longer counts against a");
    }
}
