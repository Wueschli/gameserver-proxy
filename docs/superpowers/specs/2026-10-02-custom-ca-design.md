# Custom CA support for the HTTP clients — design

Date: 2026-10-02. Status: approved (owner asked for autonomous progress through the
superpowers stages). First step of the "native TLS" umbrella in `HANDOVER.md`.

## Problem

Every outbound HTTP client in the fleet uses `reqwest` with the `rustls-tls` feature,
which trusts only the Mozilla roots compiled into the binary (`webpki-roots`). No flag
or environment variable adds a CA. So the supported "`wayhouse-controller` behind TLS"
pattern (docs/12) only works with a **publicly trusted** certificate; a private or
internal CA, or a self-signed certificate, fails verification.

## Goal and success criteria

- An operator can point any of the five binaries at a PEM file of extra CA
  certificates, and every outbound HTTPS call that binary makes trusts them.
- The built-in Mozilla roots keep working (the flag is **additive**), so nothing that
  works today breaks, with or without the flag.
- A bad `--ca-file` (missing, unreadable, no certificates, malformed certificate) is a
  **startup error** naming the file, never a silent fallback.
- Proven end to end: a real binary talks to `wayhouse-controller` through a TLS terminator
  whose certificate is signed by a private CA — failing without the flag, passing with
  it.

Out of scope (later rows of the same umbrella): TLS served by `wayhouse-controller` itself,
TLS for HA/adopt traffic (still hard-coded `http://`), client certificates (mTLS), and
reading the OS certificate store.

## Approaches considered

1. **`--ca-file <PEM>` on each binary, applied through one shared helper (chosen).**
   Explicit, works in distroless containers (no system store needed), testable,
   and additive to the built-in roots.
2. **Switch `reqwest` to `rustls-tls-native-roots` (+ keep webpki).** Zero flags: the
   OS store and `SSL_CERT_FILE` would just work. Rejected for now: every
   `Client::new()` would load and parse the whole system store from disk, and several
   call sites build a client per request or per reconnect (HA forwarding, SSE
   reconnect loops); it also changes trust for every deployment implicitly. Can be
   added later behind the same helper if wanted.
3. **Thread a `Certificate` list through every call site explicitly.** Purest, but
   touches every function signature between `main` and ~20 call sites for no
   behavioural gain over a set-once global.

## Design

### New crate `crates/wayhouse-http`

A tiny library crate whose only dependency is `reqwest` (plus `thiserror`). It is the
single place outbound HTTP clients get built. It does **not** depend on
`wayhouse-core`/`wayhouse-config`, so `wayhouse-aggregator` and `wayhouse-ui` stay decoupled from the
data-plane crates.

```rust
/// Read `path` as a PEM bundle and install its certificates as extra roots for every
/// client built through this crate. Call once from `main`, before any client is built.
pub fn init_ca_file(path: &Path) -> Result<usize, CaError>;   // returns cert count

/// A `reqwest::ClientBuilder` with the extra roots (if any) already added.
pub fn builder() -> reqwest::ClientBuilder;

/// `builder().build()`; the drop-in for `reqwest::Client::new()`.
pub fn client() -> reqwest::Client;
```

- State: a process-global `OnceLock<Vec<reqwest::Certificate>>`. Not set → no extra
  roots, i.e. exactly today's behaviour. A second `init_ca_file` is an error
  (`AlreadyInitialised`), never a silent overwrite.
- Validation in `init_ca_file`: read the file (error names the path), parse with
  `Certificate::from_pem_bundle`, reject an empty result (`NoCertificates`), then build
  one throwaway client so a certificate that parses as PEM but is not a valid X.509
  DER fails here (`reqwest` only adds roots to the rustls store at `build()`).
- `client()` panics only if `build()` fails, which after `init_ca_file` validated the
  same certificates cannot happen; the `expect` message says so. This matches the
  existing `reqwest::Client::new()` behaviour (which also panics on build failure).
- Semantics deliberately unchanged at call sites: each call still builds a fresh client
  where it built one before (no new shared connection pool), so keep-alive, timeout and
  SSE behaviour stay exactly as they are.

### Binaries

Each of `wayhouse`, `wayhouse-agent`, `wayhouse-controller`, `wayhouse-aggregator`, `wayhouse-ui` gains

```
--ca-file <PATH>   PEM file of extra CA certificates to trust for outbound HTTPS,
                   in addition to the built-in Mozilla roots
```

`main` calls `wayhouse_http::init_ca_file` before anything else that can make a request;
an error exits non-zero with the message (same path as other startup errors).

Every production `reqwest::Client::new()` / `reqwest::Client::builder()` in those five
crates becomes `wayhouse_http::client()` / `wayhouse_http::builder()`. That includes `wayhouse`'s
resolver, Consul and Kubernetes discovery clients (the Kubernetes source's own cluster
CA is still added on top) and `wayhouse-controller`'s HA/adopt clients (harmless today since
they are `http://`, and ready for the TLS-for-HA row). Test-only clients and
`wayhouse-fleet-tests`/`wayhouse-bench` keep plain `reqwest`.

### Errors

```rust
pub enum CaError {
    Read { path: PathBuf, source: io::Error },
    Parse { path: PathBuf, source: reqwest::Error },
    NoCertificates { path: PathBuf },
    AlreadyInitialised,
}
```

Every variant's message names the file (`--ca-file <path>: …`).

## Testing

- `wayhouse-http` unit tests (in-process, `127.0.0.1:0` only), using a committed test-only
  CA + `localhost` leaf (PEM fixtures under `crates/wayhouse-http/tests/fixtures/`, long
  validity, private key clearly marked test-only) and a `tokio-rustls` server:
  - a plain client **fails** the handshake against the server (proves the fixture CA
    is not publicly trusted, so the positive test is not vacuous);
  - a builder with the fixture CA succeeds;
  - missing file, file with no certificates, and a malformed certificate each give the
    matching `CaError`.
  The global `OnceLock` is exercised through a non-global inner function
  (`load_ca_file(path) -> Result<Vec<Certificate>>` + `builder_with(&[Certificate])`) so
  tests do not fight over process state; `init_ca_file` is a thin wrapper.
- `wayhouse-fleet-tests` (real binaries):
  - for each of the five binaries, `--ca-file /nonexistent` exits non-zero and the
    output names the file (proves the flag is wired everywhere);
  - end to end: a TLS terminator (tokio-rustls, fixture cert) in front of a real
    `wayhouse-controller`; `wayhouse --check --controller https://localhost:<port>` fails
    without `--ca-file` and succeeds with it.

## Docs

- docs/12 "wayhouse-controller behind TLS": replace the "publicly trusted certificates only"
  limit with how to use `--ca-file`.
- `HANDOVER.md`: drop the "Custom CA support" follow-up row, update the native-TLS
  umbrella row and "Most recent landings".
- docs/09: ADR 26 (shared `wayhouse-http` client builder, additive `--ca-file`).
- AGENTS.md repository layout: add `wayhouse-http`.
