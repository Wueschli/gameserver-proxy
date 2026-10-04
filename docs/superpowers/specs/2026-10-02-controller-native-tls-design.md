# Native TLS for `gsp-controller` — design

Date: 2026-10-02. Status: approved (owner asked for autonomous progress through the
superpowers stages). Last piece of the "native TLS" umbrella in `HANDOVER.md`, after
`--ca-file` and TLS-capable HA peers.

## Problem

`gsp-controller` serves plain HTTP only. The supported way to encrypt its traffic is a
reverse proxy in front of every replica (docs/12 "gsp-controller behind TLS"). That is an
extra moving part per replica, and it is a trust boundary that sees every bearer token,
config and registration in the clear.

## Goal and success criteria

- `gsp-controller --tls-cert <chain.pem> --tls-key <key.pem>` serves HTTPS on `--listen`
  with no proxy. Every route works over it, including the SSE streams.
- Fleet clients reach it with an `https://` URL; with a private CA they add `--ca-file`
  (built 2026-10-02). HA replicas reach each other with `--ha-peers id=https://…`.
- The certificate can be renewed **without a restart**: replacing the files is picked up
  within 30 s. A broken replacement keeps the current certificate and logs an error.
- Bad files at startup (missing, unreadable, no certificate, no/invalid key, key not
  matching the certificate) stop the controller with an error naming the file.
- Proven end to end: `gsp --check` against a natively-TLS controller (fails without
  `--ca-file`, passes with it), and a 3-replica HA cluster with native TLS on every
  replica replicates.

Out of scope: TLS on `gsp-aggregator`, `gsp-ui` and `gsp`'s admin API (follow-up — the
serving code is shared, so each is a flag plus wiring); client certificates (mTLS);
ACME; serving HTTP and HTTPS on the same port.

## Approaches considered

1. **A TLS `axum::serve::Listener` in `gsp-http`, handshakes in background tasks
   (chosen).** `axum::serve(listener, app)` stays as is; only the listener type changes.
   Uses `tokio-rustls`/`rustls` (ring) already in the lockfile, so no new third-party
   crate.
2. **`axum-server` with its rustls feature.** Mature and has cert reload, but a new
   dependency tree and a different serve API (graceful shutdown, `Handle`) than every
   other binary in the fleet.
3. **Handshake inside `Listener::accept`.** Simplest, but one slow or malicious client
   stalls all accepts (head-of-line blocking) — rejected.

## Design

### `gsp_http::tls` (new module in `crates/gsp-http`)

```rust
pub struct TlsFiles { pub cert: PathBuf, pub key: PathBuf }

/// Load and validate the pair once (startup check); errors name the file.
pub fn load_certified_key(files: &TlsFiles) -> Result<CertifiedKey, TlsError>;

/// A rustls server config whose certificate can be swapped at runtime.
pub struct ReloadingCert { /* ArcSwap<Arc<CertifiedKey>> + files + last mtimes */ }
impl ReloadingCert {
    pub fn new(files: TlsFiles) -> Result<Arc<Self>, TlsError>;
    /// Re-read the files if either mtime changed; on error keep the old key and return it.
    pub fn reload_if_changed(&self) -> Result<bool, TlsError>;
}
impl rustls::server::ResolvesServerCert for ReloadingCert { … }

/// Bind `addr` and serve TLS: a TCP accept loop spawns one task per connection that
/// runs the handshake (10 s timeout) and hands finished streams to `accept()`.
pub struct TlsListener { … }
impl TlsListener {
    pub async fn bind(addr: SocketAddr, cert: Arc<ReloadingCert>) -> io::Result<Self>;
}
impl axum::serve::Listener for TlsListener { type Io = TlsStream<TcpStream>; type Addr = SocketAddr; }

/// Poll `cert` every `every` (30 s in production), logging reloads and failures.
pub fn spawn_reloader(cert: Arc<ReloadingCert>, every: Duration) -> JoinHandle<()>;
```

- rustls `ServerConfig` via `builder_with_provider(ring)`, safe default protocol
  versions, no client auth, ALPN `h2` + `http/1.1` (axum serves both).
- Key matching: `CertifiedKey::keys_match()` (rustls 0.23.45, compares the key's SPKI
  with the leaf certificate's); `InconsistentKeys::KeyMismatch` → `TlsError::KeyMismatch`.
- A failed handshake or timeout drops that connection at `debug` level; it never
  reaches `axum::serve`.
- Errors: `TlsError::{Read{path,source}, NoCertificate{path}, NoKey{path}, BadKey{path,
  source}, KeyMismatch{cert,key}}`; message text starts `--tls-cert <path>:` /
  `--tls-key <path>:`, cause via `source()` (the convention `CaError` now follows).

`gsp-http` gains normal deps on `axum`, `tokio` (net, time), `tokio-rustls`,
`arc-swap`, `tracing` — all workspace deps already. Still no `gsp-core`/`gsp-config`.

### `gsp-controller`

`--tls-cert` / `--tls-key` (clap `requires` each other). When set: build
`ReloadingCert` right after `--ca-file` (startup error on bad files), `TlsListener::bind`
instead of `TcpListener::bind`, `spawn_reloader(cert, 30 s)`. Log
`serving HTTPS on <addr>`. Nothing else in the controller changes: HA peers and clients
choose `https://` by URL.

## Testing

- `gsp-http` (`tests/tls_server.rs`), real loopback TLS:
  - a `TlsListener` + tiny axum app answers a client trusting the fixture CA;
  - a stalled client (TCP connect, no handshake) does not block a second client;
  - `load_certified_key`: missing cert, cert file without certificates, key file
    without a key, mismatched key → matching `TlsError`, message names the file;
  - rotation: start on leaf A (CA A), overwrite the files with leaf B (CA B, new
    fixtures), `reload_if_changed()` → `true`, a client trusting only CA B connects;
    overwrite with garbage → `Err`, and a client trusting CA B still connects.
- `gsp-fleet-tests`:
  - `controller_native_tls.rs`: controller with `--tls-cert/--tls-key` (fixture leaf);
    `gsp --check --controller https://localhost:<port>` fails on the certificate without
    `--ca-file`, passes with it; `--tls-cert` without `--tls-key` exits non-zero.
  - `ha_tls.rs` gains `three_replicas_replicate_over_native_tls`: same cluster as the
    terminator test but each replica serves TLS itself.

## Docs

docs/12: new "Native TLS" subsection at the top of "gsp-controller behind TLS" (flags,
rotation, the proxy pattern stays valid for the other services); HANDOVER (umbrella row →
follow-up row for aggregator/UI/admin TLS); ADR 27 in docs/09; AGENTS layout line for
`gsp-http`.
