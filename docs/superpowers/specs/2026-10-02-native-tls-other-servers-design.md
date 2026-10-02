# Native TLS for `gsp-aggregator`, `gsp-ui` and `gsp`'s admin API — design

Date: 2026-10-02. Status: approved (owner asked for the follow-up to be done the same way
as the controller, one PR per server, merged on green CI). Builds on
`2026-10-02-controller-native-tls-design.md` (ADR 27), whose `gsp_http::tls` module does
the serving; this design only decides how each server is configured and wired.

## Problem

After ADR 27 only `gsp-controller` serves HTTPS itself. The aggregator, the UI and every
`gsp` instance's admin API still need a reverse proxy for TLS (docs/12). The UI is the
most exposed: operators log in to it with a password and it hands out a session cookie.

## Goal and success criteria

- `gsp-aggregator --tls-cert/--tls-key` and `gsp-ui --tls-cert/--tls-key` serve HTTPS on
  `--listen`, same semantics as the controller: both or neither, bad files are a startup
  error naming the file, renewal picked up within 30 s, a broken replacement keeps the
  current certificate.
- `gsp`'s admin API serves HTTPS when `settings.admin.tls` names a cert and key.
- Proven end to end on the real binaries:
  - aggregator: a `gsp` pushes to `--aggregator https://…` (with `--ca-file`) and the
    aggregator's `/fleet/pools` over HTTPS shows it; `--tls-cert` alone is refused.
  - UI: login over HTTPS sets a `Secure` session cookie and the cookie works for an
    authenticated route; the plain-HTTP UI still sets no `Secure` (it would break login
    on `http://`).
  - admin API: `/healthz` over HTTPS; the aggregator fans an intent verb out to an
    instance whose admin API is HTTPS (the instance reports an `https://` admin URL).

Out of scope: mTLS, ACME, HTTP and HTTPS on one port, redirect from HTTP.

## Decisions

### One serve helper, not four copies

`gsp_http::tls` gains

```rust
#[derive(clap::Args)]
pub struct TlsArgs { tls_cert: Option<PathBuf>, tls_key: Option<PathBuf> } // both or neither
impl TlsArgs { pub fn load(&self) -> Result<Option<Arc<ReloadingCert>>, TlsError>; }

pub const RELOAD_EVERY: Duration = Duration::from_secs(30);
/// HTTPS through TlsListener + reloader when `cert` is set, plain TcpListener otherwise.
pub async fn serve(addr, app: Router, cert: Option<Arc<ReloadingCert>>, name) -> io::Result<()>;
```

The controller moves onto it (behaviour unchanged). `clap` becomes a `gsp-http`
dependency; every crate that depends on `gsp-http` already builds clap, so the build
graph does not grow.

### `gsp` admin API: in the YAML, not flags

`settings.admin.listen` and `settings.admin.auth_token` live in the config file, so the
TLS pair does too:

```yaml
settings:
  admin:
    listen: 0.0.0.0:9900
    tls:
      cert: /etc/gsp/tls/fullchain.pem
      key: /etc/gsp/tls/privkey.pem
```

`tls` is all-or-nothing by type (both fields required). Like `listen`, it is read once at
startup; a config reload that changes it is silently not applied, exactly as for `listen`
(the admin listener is never rebound). The files themselves renew every 30 s as elsewhere.
With TLS on, the admin URL `gsp` reports to the aggregator becomes `https://`, so the
aggregator's fan-out (built on `gsp_http::client`, which honours `--ca-file`) reaches it.
Validation (`gsp --check`) loads the pair, so a bad path fails the check, not the start.

Alternative rejected: `--admin-tls-cert/--admin-tls-key` flags. It splits the admin
listener's settings across flag and file, and a controller-served config could then not
describe an instance's admin API.

### UI cookie: `Secure` exactly when serving HTTPS

`gsp-ui` with `--tls-cert` marks the session cookie (and its expiry on logout) `Secure`.
Without TLS it stays as today — a `Secure` cookie on `http://` is dropped by browsers and
login would silently loop. A UI behind a TLS-terminating proxy keeps the old behaviour;
docs/12 says so.

### UI WebSocket over HTTP/2

`TlsListener` offers ALPN h2, and `axum::serve` with the `http2` feature (on since ADR 27)
advertises extended CONNECT (RFC 8441, axum 0.8.9 `serve/mod.rs`). A browser that loaded
the UI over h2 then opens `/ws/fleet` on the same connection as `CONNECT` with
`:protocol websocket` — and the route is `get(ws_handler)`, which answers 405. The UI PR
routes it with `any(ws_handler)` (axum's documented way to accept both) and tests an
HTTP/2 WebSocket against the real router.

## Order

Three PRs, each merged before the next starts: (1) the helper + aggregator,
(2) the UI, (3) the admin API. A fourth PR carries the ADR 27 deferred review minors.
