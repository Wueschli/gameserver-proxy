//! Native TLS for the fleet's HTTP servers (`gsp-controller --tls-cert/--tls-key`).
//!
//! [`ReloadingCert`] holds the served certificate behind an `ArcSwap` and re-reads
//! the PEM files when their mtime changes, so a renewed certificate (certbot,
//! cert-manager) is picked up without a restart; a broken or half-written
//! replacement keeps the current one. [`TlsListener`] is an `axum::serve::Listener`
//! that runs each handshake in its own task, so a slow client never blocks accepts.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use arc_swap::ArcSwap;
use tokio_rustls::rustls;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::server::{ClientHello, ResolvesServerCert};
use tokio_rustls::rustls::sign::CertifiedKey;

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
