//! The one place the fleet binaries build outbound HTTP clients — and, with the
//! `server` feature, serve HTTPS themselves (the `tls` module, native TLS).
//!
//! `reqwest`'s `rustls-tls` trusts only the Mozilla roots compiled into the
//! binary. `--ca-file` adds an operator's own CAs (a private or internal CA, a
//! self-signed controller) **on top of** those roots — never instead of them.
//!
//! Deliberate simplification: the extra roots are a process-global, set once
//! from `main` via [`init_ca_file`] before any client is built, rather than
//! threaded through every call site. Not set means exactly the old behaviour.
//! [`builder`] / [`client`] still build a fresh client per call, so call sites
//! keep their existing connection and timeout semantics.

// The commit-SHA choice made by `build.rs`; compiled here only so its tests run.
#[cfg(test)]
#[path = "../build_sha.rs"]
mod build_sha;

/// Short git SHA this workspace was built from (`build.rs`), or `"unknown"` when
/// `.git` was unavailable (a source tarball).
pub const COMMIT: &str = env!("WAYHOUSE_GIT_SHA");

/// Largest sniffer module (`.wasm`) any hop accepts: the proxy's `POST /admin/sniffers`, and the
/// aggregator and UI routes in front of it. One constant so the three limits cannot drift.
pub const MAX_SNIFFER_MODULE_BYTES: usize = 8 * 1024 * 1024;

/// `<version> (<commit>)`: what every binary prints for `--version`, so the commit
/// is compiled into each of them (`deploy/check-image-commit.sh` looks for it).
pub const LONG_VERSION: &str = env!("WAYHOUSE_LONG_VERSION");

#[cfg(feature = "server")]
pub mod metrics;
#[cfg(feature = "server")]
pub mod policy;
pub mod protocol;
pub mod sse;
#[cfg(feature = "server")]
pub mod tls;

#[cfg(feature = "server")]
pub mod server;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use reqwest::{Certificate, Client, ClientBuilder};

static EXTRA_ROOTS: OnceLock<Vec<Certificate>> = OnceLock::new();

#[derive(Debug, thiserror::Error)]
pub enum CaError {
    #[error("--ca-file {}: cannot read the file", path.display())]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("--ca-file {}: invalid certificate", path.display())]
    Parse {
        path: PathBuf,
        source: reqwest::Error,
    },
    #[error("--ca-file {}: no PEM certificates found", path.display())]
    NoCertificates { path: PathBuf },
    #[error("--ca-file was already loaded")]
    AlreadyInitialised,
}

/// Read `path` as a PEM bundle. Non-certificate sections (e.g. a private key)
/// are ignored; an empty result or a certificate rustls rejects is an error,
/// so a bad file fails at startup rather than on the first request.
pub fn load_ca_file(path: &Path) -> Result<Vec<Certificate>, CaError> {
    let pem = std::fs::read(path).map_err(|source| CaError::Read {
        path: path.to_owned(),
        source,
    })?;
    let parse = |source| CaError::Parse {
        path: path.to_owned(),
        source,
    };
    let certs = Certificate::from_pem_bundle(&pem).map_err(parse)?;
    if certs.is_empty() {
        return Err(CaError::NoCertificates {
            path: path.to_owned(),
        });
    }
    // reqwest only adds roots to the rustls store at build time.
    builder_with(&certs).build().map_err(parse)?;
    Ok(certs)
}

/// A builder trusting the built-in roots plus `extra`, sending the
/// [`protocol::HEADER`] on every request so a peer can refuse an incompatible major.
pub fn builder_with(extra: &[Certificate]) -> ClientBuilder {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::HeaderName::from_static(protocol::HEADER),
        reqwest::header::HeaderValue::from_str(&protocol::ProtocolVersion::CURRENT.to_string())
            .expect("digits and a dot are a valid header value"),
    );
    extra.iter().cloned().fold(
        Client::builder().default_headers(headers),
        ClientBuilder::add_root_certificate,
    )
}

/// Load `path` (see [`load_ca_file`]) and trust it in every client built
/// through [`builder`] / [`client`] from now on. Returns the certificate count.
pub fn init_ca_file(path: &Path) -> Result<usize, CaError> {
    let certs = load_ca_file(path)?;
    let n = certs.len();
    EXTRA_ROOTS
        .set(certs)
        .map_err(|_| CaError::AlreadyInitialised)?;
    Ok(n)
}

/// The drop-in for `reqwest::Client::builder()`.
pub fn builder() -> ClientBuilder {
    builder_with(EXTRA_ROOTS.get().map_or(&[], Vec::as_slice))
}

/// The drop-in for `reqwest::Client::new()`.
pub fn client() -> Client {
    builder()
        .build()
        .expect("the extra roots were validated by init_ca_file, so the client builds")
}

/// `e` followed by each of its `source()`s, joined with `": "`. A
/// `reqwest::Error`'s own `Display` stops at "error sending request for url
/// (…)", hiding the cause (a refused connection, an untrusted certificate);
/// use this wherever an HTTP error becomes text. A source whose text the
/// message already ends with (at a `": "` boundary, or the whole message) is
/// skipped, since hyper/reqwest often repeat it; a short source that merely
/// appears somewhere inside an earlier message is kept.
pub fn error_chain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut out = e.to_string();
    let mut next = e.source();
    while let Some(src) = next {
        let text = src.to_string();
        if out != text && !out.ends_with(&format!(": {text}")) {
            out.push_str(": ");
            out.push_str(&text);
        }
        next = src.source();
    }
    out
}

/// Compares a presented bearer token / password against the expected one in
/// time that does not depend on *where* the two first differ, so a network
/// attacker cannot recover a secret byte by byte from response timing (which
/// `==` on `str` allows: it returns at the first mismatching byte). The loop
/// always runs over the expected secret's length, so the presented value's
/// length doesn't change the timing either; a length mismatch is folded into
/// the result instead of returning early.
pub fn token_eq(presented: &str, expected: &str) -> bool {
    let (a, b) = (presented.as_bytes(), expected.as_bytes());
    let mut diff = u8::from(a.len() != b.len());
    for (i, y) in b.iter().enumerate() {
        diff |= a.get(i).copied().unwrap_or(0) ^ y;
    }
    std::hint::black_box(diff) == 0
}

#[cfg(test)]
mod token_eq_tests {
    use super::token_eq;

    #[test]
    fn equal_tokens_match() {
        assert!(token_eq("secret-token", "secret-token"));
        assert!(token_eq("", ""));
    }

    #[test]
    fn different_tokens_do_not_match() {
        assert!(!token_eq("secret-tokeN", "secret-token"));
        assert!(!token_eq("Xecret-token", "secret-token"));
        assert!(!token_eq("secret", "secret-token"));
        assert!(!token_eq("secret-token-longer", "secret-token"));
        assert!(!token_eq("", "secret-token"));
        assert!(!token_eq("secret-token", ""));
        // A prefix padded with NULs must not match: the length check counts.
        assert!(!token_eq("secret\0\0\0\0\0\0", "secret"));
    }
}

#[cfg(test)]
mod version_tests {
    #[test]
    fn long_version_is_version_and_commit() {
        assert!(!super::COMMIT.is_empty());
        assert_eq!(
            super::LONG_VERSION,
            format!("{} ({})", env!("CARGO_PKG_VERSION"), super::COMMIT)
        );
    }
}
