# Native TLS for gsp-controller Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `gsp-controller --tls-cert/--tls-key` serves HTTPS itself, renews its certificate from disk without a restart, and works for clients and HA peers.

**Architecture:** A new `gsp_http::tls` module: certificate loading + a hot-swappable rustls cert resolver, and an `axum::serve::Listener` that runs TLS handshakes in per-connection tasks. The controller swaps `TcpListener` for it when the flags are set.

**Tech Stack:** Rust, axum 0.8.9 (`serve::Listener`), rustls 0.23.45 (ring), tokio-rustls 0.26, arc-swap 1.

**Spec:** `docs/superpowers/specs/2026-10-02-controller-native-tls-design.md`

## Global Constraints

- No new third-party crate: `axum`, `tokio`, `tokio-rustls`, `arc-swap`, `tracing` are workspace deps already; `gsp-http` must not depend on `gsp-core`/`gsp-config`.
- rustls provider: ring, via `builder_with_provider`; safe default protocol versions; no client auth; ALPN `h2`, `http/1.1`.
- Handshake timeout 10 s; reload poll every 30 s in production.
- Error text starts `--tls-cert <path>: ` or `--tls-key <path>: `; the cause only via `source()`.
- Bad files at startup are fatal; bad files on reload keep the current certificate.
- `make check` green before every commit; `cargo fmt --all` first.

## Review Focus

1. Key in another PEM encoding (SEC1 `EC PRIVATE KEY` instead of PKCS#8) → loads. → Task 1 `loads_a_sec1_key`.
2. Cert file holding a chain (leaf + intermediate/CA) → served as a chain, client verifies. → Task 2 `serves_a_chain_file`.
3. Rotation done in two steps (cert replaced before key) → mismatch on the in-between poll keeps the old cert; the next poll after the key lands switches. → Task 1 `half_rotated_pair_keeps_the_old_cert`.
4. A plain-HTTP request to the TLS port → that connection is dropped, the server keeps serving. → Task 2 `plain_http_on_the_tls_port_does_not_break_the_server`.
5. Long-lived SSE stream over native TLS → first event arrives. → Task 3 `config_subscribe_streams_over_native_tls`.

---

### Task 1: Certificate loading and hot reload (`gsp_http::tls`)

**Files:**
- Create: `crates/gsp-http/src/tls.rs` (`pub mod tls;` in lib.rs), `crates/gsp-http/tests/tls_certs.rs`
- Create fixtures: `crates/gsp-http/tests/fixtures/{ca2.pem,leaf2.pem,leaf2.key,leaf.sec1.key}` (+ README recipe: second CA, leaf2 for `localhost`/`127.0.0.1` signed by it, CA key discarded; `leaf.sec1.key` = `openssl ec -in leaf.key -out leaf.sec1.key`)
- Modify: `crates/gsp-http/Cargo.toml` (deps `tokio-rustls`, `arc-swap`, `tracing`, `tokio`, `axum`)

**Interfaces:**
- Produces: `pub struct TlsFiles { pub cert: PathBuf, pub key: PathBuf }`; `pub enum TlsError { Read{path,source:io::Error}, NoCertificate{path}, NoKey{path}, BadKey{path,source:rustls::Error}, KeyMismatch{cert,key} }`; `pub fn load_certified_key(&TlsFiles) -> Result<CertifiedKey, TlsError>`; `pub struct ReloadingCert` with `new(TlsFiles) -> Result<Arc<Self>, TlsError>`, `reload_if_changed(&self) -> Result<bool, TlsError>`, `current(&self) -> Arc<CertifiedKey>`, and `impl ResolvesServerCert`.

- [ ] **Step 1: failing tests** (`tls_certs.rs`):
  - `loads_the_fixture_pair`: `load_certified_key(leaf.pem, leaf.key)` → Ok, `cert.len() == 1`.
  - `loads_a_sec1_key`: `(leaf.pem, leaf.sec1.key)` → Ok.
  - `startup_errors_name_the_file`: missing cert → `Read`, text starts `--tls-cert <path>: `; cert file = `leaf.key` → `NoCertificate`; key file = `leaf.pem` → `NoKey`, text starts `--tls-key <path>: `; `(leaf.pem, leaf2.key)` → `KeyMismatch`. Each: `source()` text not in `to_string()`.
  - `rotation_swaps_the_served_cert`: temp dir with copies of leaf/leaf.key → `ReloadingCert::new`; `reload_if_changed()` → `Ok(false)`; overwrite both with leaf2/leaf2.key (bump mtime: `filetime` not available → sleep 10 ms then write; assert mtime changed or rewrite until it does) → `Ok(true)` and `current().cert[0]` == leaf2 DER.
  - `half_rotated_pair_keeps_the_old_cert`: overwrite cert only with leaf2 → `Err(KeyMismatch)`, `current()` still leaf; then key → `Ok(true)`, current is leaf2.
  - `garbage_on_reload_keeps_the_old_cert`: overwrite cert with `"junk"` → `Err(NoCertificate)`, current unchanged.
- [ ] **Step 2:** `cargo test -p gsp-http --test tls_certs` → compile FAIL (module missing).
- [ ] **Step 3:** implement: PEM via `rustls::pki_types::{CertificateDer, PrivateKeyDer}` `pem_slice_iter` / `from_pem_slice`; key via `ring::sign::any_supported_type`; `CertifiedKey::new(chain, key).keys_match()`; state = `ArcSwap<CertifiedKey>` + `Mutex<(SystemTime, SystemTime)>` of last mtimes, updated only on a successful swap (so a failed reload retries next poll).
- [ ] **Step 4:** tests PASS.
- [ ] **Step 5:** `cargo fmt --all && make check`; commit `feat(gsp-http): TLS certificate loading with hot reload`.

### Task 2: `TlsListener` and the reloader task

**Files:**
- Modify: `crates/gsp-http/src/tls.rs`
- Create: `crates/gsp-http/tests/tls_server.rs`

**Interfaces:**
- Consumes: Task 1's `ReloadingCert`.
- Produces: `pub struct TlsListener`; `TlsListener::bind(addr: SocketAddr, cert: Arc<ReloadingCert>) -> io::Result<TlsListener>`; `impl axum::serve::Listener for TlsListener { type Io = tokio_rustls::server::TlsStream<TcpStream>; type Addr = SocketAddr; }`; `pub fn spawn_reloader(cert: Arc<ReloadingCert>, every: Duration) -> JoinHandle<()>`; `pub const HANDSHAKE_TIMEOUT: Duration = 10 s`.

- [ ] **Step 1: failing tests** (`tls_server.rs`; app = `Router::new().route("/", get(|| async { "ok" }))` served via `axum::serve(TlsListener::bind(127.0.0.1:0, cert), app)`; client = `gsp_http::builder_with(load_ca_file(ca.pem))`):
  - `serves_https_to_a_client_that_trusts_the_ca`: GET → 200 `ok`.
  - `serves_a_chain_file`: cert file = `leaf.pem`+`ca.pem` concatenated → GET 200.
  - `a_stalled_handshake_does_not_block_others`: open a raw `TcpStream` and send nothing; then the HTTPS GET still returns within 2 s.
  - `plain_http_on_the_tls_port_does_not_break_the_server`: raw TCP write `GET / HTTP/1.1\r\n\r\n` (read until EOF/err, ignore); then HTTPS GET → 200.
  - `the_reloader_picks_up_new_files`: `spawn_reloader(cert, 100 ms)`; rotate files to leaf2; within 3 s a client trusting only `ca2.pem` gets 200.
- [ ] **Step 2:** run → compile FAIL.
- [ ] **Step 3:** implement: `bind` builds the `ServerConfig` with the `ReloadingCert` resolver and ALPN; spawns an accept loop pushing `TlsStream`s into an `mpsc::channel(64)` after `timeout(HANDSHAKE_TIMEOUT, acceptor.accept(tcp))` in a per-connection task (failures `tracing::debug!`); `accept()` receives from the channel; `local_addr` from the bound listener. `spawn_reloader` logs `info` on swap, `error` (with `error_chain`) on failure.
- [ ] **Step 4:** tests PASS (also re-run `--test tls` and `--test tls_certs`).
- [ ] **Step 5:** `cargo fmt --all && make check`; commit `feat(gsp-http): TlsListener — TLS for axum::serve without head-of-line handshakes`.

### Task 3: `gsp-controller --tls-cert/--tls-key`, end to end

**Files:**
- Modify: `crates/gsp-controller/src/main.rs` (flags with `requires`, help text: "PEM certificate chain to serve HTTPS with (requires --tls-key); re-read when the file changes"), `crates/gsp-controller/Cargo.toml` if needed.
- Create: `crates/gsp-fleet-tests/tests/controller_native_tls.rs`
- Modify: `crates/gsp-fleet-tests/tests/ha_tls.rs`, `crates/gsp-fleet-tests/src/tls_front.rs` (export `pub const TEST_LEAF`, `TEST_LEAF_KEY` paths)

**Interfaces:**
- Consumes: `gsp_http::tls::{TlsFiles, ReloadingCert, TlsListener, spawn_reloader}`.

- [ ] **Step 1: failing e2e tests** (`controller_native_tls.rs`):
  - `gsp_check_reaches_a_natively_tls_controller`: controller with `--tls-cert TEST_LEAF --tls-key TEST_LEAF_KEY`; seed config by POST over https with a `--ca-file`-equivalent client (`gsp_http::builder_with(load_ca_file(TEST_CA))`) — wait for `/healthz` over https first; `gsp --check --controller https://localhost:<p>` fails naming the certificate without `--ca-file`, passes with it.
  - `config_subscribe_streams_over_native_tls`: GET `/config/subscribe` over https; first chunk contains `event:` or `data:` within 10 s.
  - `tls_cert_without_key_is_refused`: `--tls-cert X` alone → non-zero exit, output mentions `--tls-key`.
  - `ha_tls.rs` `three_replicas_replicate_over_native_tls`: refactor `cluster()` to take a mode (terminators vs native); native = each replica `--listen 127.0.0.1:p_i --tls-cert/--tls-key --ca-file TEST_CA`, peers `https://localhost:p_i`; same assertions as the terminator test, using an https client for the writes/reads.
- [ ] **Step 2:** run → FAIL (unknown argument `--tls-cert`).
- [ ] **Step 3:** wire: after `--ca-file`, if `tls_cert` set: `ReloadingCert::new(TlsFiles{..})?`, `TlsListener::bind`, `spawn_reloader(cert, 30 s)`, `info!("serving HTTPS on {addr}")`; else the existing `TcpListener` path. Both branches end in `axum::serve(listener, app)`.
- [ ] **Step 4:** run the new tests + `ha_tls` → PASS.
- [ ] **Step 5:** `cargo fmt --all && make check`; commit `feat(controller): --tls-cert/--tls-key serve HTTPS natively, with hot reload`.

### Task 4: Docs

**Files:** `docs/12-deployment.md` (new "Native TLS" subsection first in "gsp-controller behind TLS": flags, rotation (30 s, broken files keep the old cert), HA with native TLS, the proxy pattern still for aggregator/UI/admin; adjust the "Limits"), `docs/09-technology-choices.md` (ADR 27), `HANDOVER.md` (umbrella row → "TLS for aggregator / UI / gsp admin API" follow-up; Resume here; Most recent landings), `AGENTS.md` (gsp-http line mentions the server side).

- [ ] **Step 1:** edit; cite the tests that prove each claim.
- [ ] **Step 2:** `make check`; commit `docs: native TLS for gsp-controller`.
