//! Typed errors for the `gsp-core` public API, so callers can match on the
//! failure kind instead of string-searching an `anyhow` chain.

use std::net::SocketAddr;

use crate::pool::PickError;

/// Why proxying one TCP connection failed before the byte pump started
/// ([`handle_tcp`](crate::proxy::handle_tcp) /
/// [`handle_tcp_target`](crate::proxy::handle_tcp_target)).
#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    /// The pool had no backend to hand out.
    #[error(transparent)]
    Pick(#[from] PickError),
    /// The TCP connect to the backend / target failed.
    #[error("connect to backend {addr} failed: {source}")]
    Connect {
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },
    /// The TCP connect did not complete within the connect timeout.
    #[error("connect to backend {addr} timed out")]
    ConnectTimeout { addr: SocketAddr },
    /// Writing the PROXY protocol header to the backend / target failed.
    /// `role` is `"backend"` (pool route) or `"target"` (resolver target).
    #[error("write PROXY header to {role} {addr} failed: {source}")]
    ProxyHeader {
        role: &'static str,
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },
}

/// Why a listener accept loop could not start.
#[derive(Debug, thiserror::Error)]
pub enum ListenerError {
    /// Binding the listening socket (or registering it with tokio) failed.
    #[error(transparent)]
    Bind(#[from] std::io::Error),
}

/// Why a [`BackendSource`](crate::discovery::BackendSource) could not be built
/// or could not produce its address set.
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    /// The adapter could not be constructed from its config (bad HTTP client
    /// setup, missing tunnel registry, …). Carries the original cause chain.
    #[error("{0}")]
    Build(Box<dyn std::error::Error + Send + Sync>),
    /// The backing service could not be reached or the lookup itself failed
    /// (DNS error, connection refused, timeout, …).
    #[error("{context}: {cause}")]
    Unreachable { context: String, cause: String },
    /// The service answered, but not with something usable (error status,
    /// undecodable body, an address that does not parse, no addresses).
    #[error("{context}: {cause}")]
    BadResponse { context: String, cause: String },
    /// A pinned peer identity does not match what the registry reports.
    #[error(
        "origin {origin:?} is currently registered with a different pubkey than \
         backend_sources pins (expected {expected:?}, got {got:?}) — refusing to trust it"
    )]
    PubkeyMismatch {
        origin: String,
        expected: String,
        got: String,
    },
}

impl SourceError {
    /// Wrap any error as a [`SourceError::Build`], keeping its source chain.
    pub fn build(e: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Self {
        SourceError::Build(e.into())
    }
}
