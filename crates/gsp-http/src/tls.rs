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
    load_with_digest(files).map(|(key, _)| key)
}

/// A digest of the pair's bytes, to tell a same-stamp rewrite from no change.
/// Not cryptographic: nobody is forging a collision against a cert reload.
fn digest(cert_pem: &[u8], key_pem: &[u8]) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    cert_pem.hash(&mut h);
    key_pem.hash(&mut h);
    h.finish()
}

fn load_with_digest(files: &TlsFiles) -> Result<(CertifiedKey, u64), TlsError> {
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
    let digest = digest(&cert_pem, &key_pem);
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
    Ok((certified, digest))
}

/// What "this file changed" is judged by: mtime, size and — on Unix — inode and
/// ctime. ctime cannot be set from user space, so a rewrite that restores the
/// mtime (`cp -p`, `touch -r`) still shows. On a coarse-timestamp filesystem
/// ctime is as coarse as mtime; there a same-length, same-inode rewrite within
/// one tick of the last load leaves the stamp unchanged, so while the stamp is
/// that recent ([`RACY_WINDOW`]) the files' bytes are compared too.
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
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt as _;
    let m = std::fs::metadata(path).ok()?;
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

/// The coarsest timestamp tick we guard against (FAT: 2 s). A stamp this close
/// to the last check cannot yet rule out a later rewrite with the same stamp.
const RACY_WINDOW: Duration = Duration::from_secs(2);

/// The most recent mtime/ctime in the pair.
fn newest(stamp: &PairStamp) -> Option<SystemTime> {
    [stamp.0, stamp.1]
        .into_iter()
        .flatten()
        .flat_map(|f| {
            #[cfg(unix)]
            let ctime = {
                let (secs, nanos) = f.ctime;
                u64::try_from(secs)
                    .ok()
                    .map(|s| SystemTime::UNIX_EPOCH + Duration::new(s, nanos as u32))
            };
            #[cfg(not(unix))]
            let ctime = None;
            [f.mtime, ctime]
        })
        .flatten()
        .max()
}

/// Whether an unchanged stamp may still hide a rewrite, judged at `checked_at`.
fn is_racy(stamp: &PairStamp, checked_at: SystemTime) -> bool {
    newest(stamp).is_some_and(|t| t + RACY_WINDOW > checked_at)
}

/// What the last good load looked like.
struct Loaded {
    stamp: PairStamp,
    digest: u64,
    /// When the pair was last confirmed to match `digest`.
    checked_at: SystemTime,
}

fn stamps(files: &TlsFiles) -> PairStamp {
    (stamp(&files.cert), stamp(&files.key))
}

/// The served certificate, swappable at runtime (see the module doc).
pub struct ReloadingCert {
    files: TlsFiles,
    current: ArcSwap<CertifiedKey>,
    /// The pair last loaded successfully; a failed reload leaves it, so the next
    /// poll tries again.
    loaded: Mutex<Loaded>,
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
        let checked_at = SystemTime::now();
        let (key, digest) = load_with_digest(&files)?;
        Ok(Arc::new(Self {
            files,
            current: ArcSwap::from_pointee(key),
            loaded: Mutex::new(Loaded {
                stamp,
                digest,
                checked_at,
            }),
        }))
    }

    /// Re-read the pair if either file changed (see [`FileStamp`]) since the last
    /// good load, including a same-stamp rewrite right after it. `Ok(true)` = swapped; on `Err` the current certificate stays.
    pub fn reload_if_changed(&self) -> Result<bool, TlsError> {
        let stamp = stamps(&self.files);
        let mut loaded = self.loaded.lock().unwrap_or_else(PoisonError::into_inner);
        if loaded.stamp == stamp {
            let now = SystemTime::now();
            if !is_racy(&stamp, loaded.checked_at) {
                return Ok(false);
            }
            // Too soon after the last check for the stamp to prove "unchanged".
            let same = read(self.files.cert_name, &self.files.cert)
                .and_then(|c| Ok((c, read(self.files.key_name, &self.files.key)?)))
                .is_ok_and(|(c, k)| digest(&c, &k) == loaded.digest);
            if same {
                loaded.checked_at = now;
                return Ok(false);
            }
        }
        let now = SystemTime::now();
        let (key, digest) = load_with_digest(&self.files)?;
        self.current.store(Arc::new(key));
        *loaded = Loaded {
            stamp,
            digest,
            checked_at: now,
        };
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
///
/// A source may also open at most `new_per_source_per_sec` connections a second
/// on average, with bursts up to `new_per_source_burst` (a token bucket per
/// source); without it a source could cycle connects under its pending cap
/// forever. `new_per_source_per_sec: 0.0` turns the rate limit off.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HandshakeLimits {
    pub client_hello_timeout: Duration,
    pub handshake_timeout: Duration,
    pub max_pending: usize,
    pub max_pending_per_source: usize,
    pub new_per_source_per_sec: f64,
    pub new_per_source_burst: u32,
}

impl Default for HandshakeLimits {
    fn default() -> Self {
        Self {
            client_hello_timeout: CLIENT_HELLO_TIMEOUT,
            handshake_timeout: HANDSHAKE_TIMEOUT,
            max_pending: 512,
            max_pending_per_source: 16,
            new_per_source_per_sec: 20.0,
            new_per_source_burst: 64,
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

/// Why a handshake was refused at the door.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Refused {
    /// The source already has `max_pending_per_source` handshakes in flight.
    Cap,
    /// The source is opening connections faster than its rate allows.
    Rate,
}

/// One source's token bucket (see [`HandshakeLimits`]).
struct Bucket {
    tokens: f64,
    at: Instant,
}

/// Above this many tracked sources, buckets that have refilled completely (they
/// carry no information) are dropped; if that is not enough the table is cleared,
/// which only ever lets a flooder have a fresh burst.
const MAX_BUCKETS: usize = 8192;

/// The pending-handshake bookkeeping (see [`HandshakeLimits`]): ids in admission
/// order, so the oldest is the first.
struct Pending {
    max_pending: usize,
    max_pending_per_source: usize,
    rate: f64,
    burst: f64,
    buckets: HashMap<SourceKey, Bucket>,
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
            rate: limits.new_per_source_per_sec.max(0.0),
            burst: f64::from(limits.new_per_source_burst.max(1)),
            buckets: HashMap::new(),
            order: BTreeMap::new(),
            per_source: HashMap::new(),
            next_id: 0,
        }
    }

    /// Admit a handshake from `key`: its id and the id of the handshake evicted
    /// to make room, or why `key` is refused (at its cap, or over its rate).
    fn admit(&mut self, key: SourceKey) -> Result<(u64, Option<u64>), Refused> {
        self.admit_at(key, Instant::now())
    }

    /// Spend one token of `key`'s bucket; `false` when it is empty.
    fn take_token(&mut self, key: SourceKey, now: Instant) -> bool {
        if self.rate <= 0.0 {
            return true;
        }
        if self.buckets.len() >= MAX_BUCKETS && !self.buckets.contains_key(&key) {
            let (rate, burst) = (self.rate, self.burst);
            self.buckets
                .retain(|_, b| b.tokens + rate * now.duration_since(b.at).as_secs_f64() < burst);
            if self.buckets.len() >= MAX_BUCKETS {
                self.buckets.clear();
            }
        }
        let b = self.buckets.entry(key).or_insert(Bucket {
            tokens: self.burst,
            at: now,
        });
        let refill = self.rate * now.saturating_duration_since(b.at).as_secs_f64();
        b.tokens = (b.tokens + refill).min(self.burst);
        b.at = now;
        if b.tokens < 1.0 {
            return false;
        }
        b.tokens -= 1.0;
        true
    }

    fn admit_at(&mut self, key: SourceKey, now: Instant) -> Result<(u64, Option<u64>), Refused> {
        if self.per_source.get(&key).copied().unwrap_or(0) >= self.max_pending_per_source {
            return Err(Refused::Cap);
        }
        if !self.take_token(key, now) {
            return Err(Refused::Rate);
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
        Ok((id, evicted))
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
                        Err(why) => {
                            h.warn_throttled(match why {
                                Refused::Cap => "per source, too many pending",
                                Refused::Rate => "per source, too many new connections",
                            });
                            None
                        }
                        Ok((id, evicted)) => {
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

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures")
                .join(name),
        )
        .unwrap()
    }

    /// A loaded pair, then `leaf2.key` written over `key.pem` (same length) and
    /// the recorded stamp forced to match: what a coarse-timestamp filesystem
    /// shows for a rewrite within one tick of the load.
    fn rewritten_with_unchanged_stamp() -> (tempfile::TempDir, Arc<ReloadingCert>) {
        let dir = tempfile::tempdir().unwrap();
        let files = TlsFiles::new(dir.path().join("cert.pem"), dir.path().join("key.pem"));
        std::fs::write(&files.cert, fixture("leaf.pem")).unwrap();
        std::fs::write(&files.key, fixture("leaf.key")).unwrap();
        let cert = ReloadingCert::new(files.clone()).unwrap();
        std::fs::write(&files.key, fixture("leaf2.key")).unwrap();
        cert.loaded.lock().unwrap().stamp = stamps(&files);
        (dir, cert)
    }

    #[test]
    fn a_same_stamp_rewrite_right_after_the_load_is_seen() {
        let (_dir, cert) = rewritten_with_unchanged_stamp();
        // The new key doesn't match the old certificate: noticing the change
        // means trying the pair and refusing it; Ok(false) would mean it went
        // unseen.
        let seen = cert.reload_if_changed();
        assert!(
            matches!(seen, Err(TlsError::KeyMismatch { .. })),
            "{seen:?}"
        );
    }

    #[test]
    fn an_unchanged_pair_in_the_racy_window_is_not_reloaded() {
        let dir = tempfile::tempdir().unwrap();
        let files = TlsFiles::new(dir.path().join("cert.pem"), dir.path().join("key.pem"));
        std::fs::write(&files.cert, fixture("leaf.pem")).unwrap();
        std::fs::write(&files.key, fixture("leaf.key")).unwrap();
        let cert = ReloadingCert::new(files).unwrap();
        assert!(!cert.reload_if_changed().unwrap());
    }

    #[test]
    fn once_the_stamp_is_older_than_the_window_the_stamp_alone_decides() {
        let (_dir, cert) = rewritten_with_unchanged_stamp();
        // Last checked long after the files' newest timestamp: any later write
        // would carry a newer stamp, so an equal one means no change.
        cert.loaded.lock().unwrap().checked_at = SystemTime::now() + Duration::from_secs(3600);
        assert!(!cert.reload_if_changed().unwrap());
    }

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
        assert_eq!(p.admit(a).unwrap_err(), Refused::Cap);
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

    fn rate_limited(per_sec: f64, burst: u32) -> Pending {
        Pending::new(&HandshakeLimits {
            max_pending: 1000,
            max_pending_per_source: 1000,
            new_per_source_per_sec: per_sec,
            new_per_source_burst: burst,
            ..HandshakeLimits::default()
        })
    }

    #[test]
    fn a_source_cycling_connects_under_its_cap_is_rate_limited() {
        let mut p = rate_limited(10.0, 3);
        let a = source_key(ip("192.0.2.1"));
        let t0 = Instant::now();
        for _ in 0..3 {
            let (id, _) = p.admit_at(a, t0).expect("within the burst");
            p.release(id); // finished at once: the pending cap never bites
        }
        assert_eq!(p.admit_at(a, t0).unwrap_err(), Refused::Rate, "burst spent");
        // Another source has its own bucket.
        assert!(p.admit_at(source_key(ip("192.0.2.2")), t0).is_ok());
        // 100 ms at 10/s is one token back.
        let t1 = t0 + Duration::from_millis(100);
        assert!(p.admit_at(a, t1).is_ok());
        assert!(p.admit_at(a, t1).is_err());
    }

    #[test]
    fn the_bucket_never_holds_more_than_the_burst() {
        let mut p = rate_limited(10.0, 2);
        let a = source_key(ip("192.0.2.1"));
        let t0 = Instant::now();
        p.admit_at(a, t0).unwrap();
        // A long quiet spell refills to the burst, not beyond.
        let later = t0 + Duration::from_secs(3600);
        assert!(p.admit_at(a, later).is_ok());
        assert!(p.admit_at(a, later).is_ok());
        assert!(p.admit_at(a, later).is_err());
    }

    #[test]
    fn a_rate_of_zero_turns_the_limit_off() {
        let mut p = rate_limited(0.0, 1);
        let a = source_key(ip("192.0.2.1"));
        let t0 = Instant::now();
        for _ in 0..200 {
            let (id, _) = p.admit_at(a, t0).unwrap();
            p.release(id);
        }
        assert!(p.buckets.is_empty());
    }

    #[test]
    fn the_bucket_table_stays_bounded() {
        let mut p = rate_limited(1.0, 1);
        let t0 = Instant::now();
        for n in 0..(MAX_BUCKETS as u64 * 2) {
            let _ = p.admit_at(SourceKey::V6(n), t0);
        }
        assert!(p.buckets.len() <= MAX_BUCKETS);
    }
}
