# Security review — 2026-10

A time-boxed review of the whole workspace after the repository went public
(2026-10-03). Scope: auth on the admin / controller / aggregator / UI APIs,
token handling, TLS, input parsing, DoS limits, `unsafe`, and the CI
workflow. Findings are ranked by severity; line numbers are as of the commit
that adds this file.

Severity scale: **High** = remotely exploitable against a default-ish
deployment; **Medium** = exploitable with a precondition (network position,
operator choice, valid credentials); **Low** = hardening / defence in depth.

## Fixed in this PR

| # | Severity | Finding | Where |
|---|----------|---------|-------|
| F1 | Medium | Bearer tokens and the UI password were compared with `==`, which returns at the first mismatching byte. An attacker who can reach any authenticated API can, in principle, recover the token byte by byte from response timing. All ten sites now use `wayhouse_http::token_eq`, a length-checked constant-time comparison (unit-tested). | `crates/wayhouse/src/admin.rs:86`, `crates/wayhouse-controller/src/{auth.rs:34, intent/api.rs:100, peers/api.rs:150, proxy_peers/api.rs:142, addresses/api.rs:57, adopt.rs:80, ha/routes.rs:42}`, `crates/wayhouse-aggregator/src/auth.rs:35`, `crates/wayhouse-ui/src/api.rs:217`; helper in `crates/wayhouse-http/src/lib.rs` |
| F2 | Medium | The CI workflow had no `permissions:` block, so every job got the repository's default `GITHUB_TOKEN` scope. No job writes anything, so the workflow now declares `permissions: contents: read` at the top level. (Fork PRs already get a read-only token; this mainly protects `push`/`schedule`/`workflow_dispatch` runs from a compromised action or dependency.) | `.github/workflows/ci.yml:36` |
| F3 | Medium | `wayhouse-ui --users-file` login rejected an unknown username instantly but ran Argon2 for a known one, so response timing enumerated usernames. An unknown user now verifies against a dummy Argon2 hash first. | `crates/wayhouse-ui/src/api.rs:201`, `crates/wayhouse-ui/src/users.rs` (`verify_against_dummy`) |

Timing attacks over a network are noisy and need many samples, so F1 is
Medium by this scale (it needs network position and many samples), though
the impact is full admin takeover. It is cheap to fix and every auth path
shared the same pattern.

## Open findings (not fixed here)

| # | Severity | Finding | Where | Suggested fix |
|---|----------|---------|-------|---------------|
| O1 | Medium | Every API is **open when no token is configured** (`wayhouse` admin, controller, aggregator, UI without `--ui-password`/`--users-file`). Defaults bind loopback (`127.0.0.1:990x`), which is safe, but nothing warns when an operator binds a non-loopback address without a token. | `crates/wayhouse-config/src/lib.rs:227`, `crates/wayhouse-controller/src/auth.rs:24`, `crates/wayhouse-aggregator/src/auth.rs:25`, `crates/wayhouse-ui/src/api.rs:193` | Log a `warn!` at startup (or refuse, behind an `--insecure-no-auth` flag) when the listen address is not loopback and no token/password is set. |
| O2 | Medium | UI sessions **never expire** and the store is unbounded. A stolen cookie is valid until the process restarts; in open mode anyone can grow the map with `POST /ui/login`. | `crates/wayhouse-ui/src/session.rs:31-72` | Store a creation/last-seen time, expire after an idle and an absolute TTL, cap the map size, add `Max-Age` to the cookie. |
| O3 | Medium | No rate limit on `POST /ui/login`. With `--users-file` each attempt runs Argon2 (CPU-heavy), so login is both a brute-force target and a cheap CPU DoS. | `crates/wayhouse-ui/src/api.rs:196`, `crates/wayhouse-ui/src/users.rs:66` | Per-IP token bucket on the login route (the data plane already has `ratelimit.rs` to borrow from); bound concurrent Argon2 verifications with a semaphore. |
| O4 | Medium | Gossip datagrams are HMAC-authenticated but carry **no freshness** (timestamp/nonce), so an on-path attacker can replay a captured health broadcast. LWW versioning limits the effect to re-asserting already-seen states, but a replayed "down" can briefly flap a backend. | `crates/wayhouse-core/src/gossip.rs:362-380` | Include a sender timestamp in the MAC'd payload and drop datagrams older than a window. |
| O5 | Low | The SSE config-subscribe clients buffer until `\n\n` with no size cap, so a malicious or broken controller can grow memory without bound. The controller is a trusted peer (pinned URL, optional TLS + `--ca-file`), hence Low. | `crates/wayhouse/src/controller_client.rs:149-168`, similar in `crates/wayhouse-agent/src/proxy_subscribe.rs` | Cap the buffer (e.g. 16 MiB) and reconnect when exceeded. |
| O6 | Low | Most third-party actions are referenced by a mutable tag or branch rather than a commit SHA (only `taiki-e/install-action` and the Trivy image are pinned). With F2 the blast radius is read-only, but a hijacked tag could still exfiltrate cache contents. | `.github/workflows/ci.yml` (see `uses:` lines) | Pin to commit SHAs with a version comment; let Dependabot bump them. |
| O7 | Low | A short or guessable token is accepted as-is; there is no minimum length or entropy check on `--auth-token`, `--ha-token`, `settings.admin.auth_token`, or the gossip PSK. | CLI args in each `main.rs`, `crates/wayhouse-config/src/lib.rs:201` | Reject (or warn on) tokens/PSKs shorter than ~32 chars at startup. |

## Status as of 2026-10-04 (main at `97cc114`, after #35)

Re-checked against current code. None of O1-O7 has been fixed. Line numbers
in the table above are from the original review and have drifted; the
current ones are below.

| # | Status | Evidence on main |
|---|--------|------------------|
| O1 | Fixed | **Fixed.** A non-loopback bind with no token is refused at startup (`--insecure-no-auth` opts out with a warning), via `wayhouse_http::policy::check_exposure`. Previously: no startup check for a non-loopback bind without a token. Every bearer middleware still passes requests through when no token is set, e.g. `crates/wayhouse-controller/src/auth.rs:24-34`, `crates/wayhouse-aggregator/src/auth.rs:25-35`. |
| O2 | Fixed | `crates/wayhouse-ui/src/session.rs`: sessions expire after an idle timeout (default 30 min, `--session-idle-timeout-secs`) and an absolute max age (default 12 h, `--session-max-age-secs`), and the store is capped (default 1000, `--max-sessions`; oldest evicted). The cookie carries `Max-Age`. |
| O3 | Fixed | `crates/wayhouse-ui/src/login_limit.rs`: `POST /ui/login` is throttled per client address (IPv6 by /64; burst 10, then one per 3 s) and per username (burst 5, then one per 12 s), answering `429` with `Retry-After`. Argon2 verification runs on the blocking pool behind a 4-permit semaphore. Behind a reverse proxy the per-address bucket sees the proxy's address, so only the per-username bucket is selective there. At the key cap new keys fail closed, so a wide spray of distinct usernames can briefly 429 a new legitimate key; usernames over 256 bytes are rejected before they are tracked. |
| O4 | Fixed | `crates/wayhouse-core/src/gossip.rs`: every datagram now carries a sender timestamp inside the MAC, and one more than 30 s from the receiver's clock is dropped (`wayhouse_gossip_stale_rejected_total`). A replay inside the 30 s window is still possible, and instances need clocks within 30 s of each other. The wire format changed, so a mixed-version mesh does not converge during a rolling upgrade. Previously: only the payload was MACed. |
| O5 | Fixed | `crates/wayhouse-http/src/sse.rs`: all seven SSE subscribers (`wayhouse`, `wayhouse-agent`, `wayhouse-controller`, `wayhouse-ui`) go through `EventBuffer`, which keeps only the unterminated tail and fails the stream past 16 MiB (`MAX_EVENT_BYTES`); each client reconnects with backoff. |
| O6 | Fixed | Every `uses:` in `.github/` is pinned to a commit SHA with the tag in a trailing comment, including the `stable`/`nightly` toolchain branches of `dtolnay/rust-toolchain`. Bumps are manual. |
| O7 | Fixed | **Fixed** for `--auth-token`, `--ha-token`, `settings.admin.auth_token` and the gossip PSK: minimum 16 bytes (`wayhouse_http::policy::MIN_SECRET_LEN`). `--ui-password` and outbound tokens are not checked. |

### New since the review (HA and IPv6 work, #24-#35)

| # | Severity | Finding | Where | Suggested fix |
|---|----------|---------|-------|---------------|
| N1 | Medium | The Raft peer endpoints (`/raft/append`, `/raft/vote`, `/raft/snapshot`, `/raft/pre-ha`, `/raft/whoami`) and the membership API (`/admin/ha/members`, add/remove/re-address) are **open when `--ha-token` / the admin token is absent**, the same pass-through as O1 but with more impact: an unauthenticated peer could append log entries (rewrite replicated config, registries and addresses), install a snapshot, or change cluster membership. **Fixed:** `--ha-peers`/`--ha-join` now refuse to start without `--ha-token` (`ha::check_ha_token`), and the O1 check covers the members API on a non-loopback bind. Previously nothing refused it. | `crates/wayhouse-controller/src/ha/routes.rs:41-45` (`ha_token`), `crates/wayhouse-controller/src/ha/members.rs:92-96` (`auth_token`), `crates/wayhouse-controller/src/main.rs:94,303` | Refuse to start with `--ha-peers`/`--ha-join` unless `--ha-token` is set (and the admin token for the members API when the listen address is non-loopback). Fold into the O1 fix. |
| N2 | Low | `/raft/*` accepts bodies up to 32 MiB (`MAX_RAFT_BODY`), well above axum's 2 MiB default. Needed for snapshot chunks and pre-HA imports, and the routes are token-gated, so it is only reachable by a token holder, or by anyone when N1 applies. | `crates/wayhouse-controller/src/ha/routes.rs:21-33` | Keep, but raise N1 first; consider a smaller limit on `/raft/vote` and `/raft/append`. **Fixed** (limits are per route now): `/raft/vote` 64 KiB, `/raft/snapshot` 4 MiB (its chunks are 256 KiB), `/raft/append` stays 32 MiB because a pre-HA import travels as one log entry. |
| N3 | Low | Raft traffic carries the bearer token and all replicated state. It is encrypted only if the peer URLs are `https://`; plain `http://` peers send the token in clear. | `crates/wayhouse-controller/src/ha/network.rs:27,64` | Document that HA peers should use `https://` and `--ca-file`, or warn on an `http://` peer when a token is set. **Fixed** (warning): `--ha-peers`, `PUT`/`POST /admin/ha/members` log a warning for any non-loopback plain-http peer. It does not refuse, so existing deployments keep starting; loopback peers are exempt. |

The other HA and IPv6 additions (`/tunnel/addresses`, intent, registry and adopt
routes) all use `wayhouse_http::token_eq` bearer checks (see the `token_eq` call sites
in `crates/wayhouse-controller/src/{addresses/api.rs:75, intent/api.rs:114,
registry.rs:344, adopt.rs:80}`), so F1 still holds. Not re-audited beyond that.

### Aggregator token split and fan-out target (2026-10-05)

| # | Severity | Finding | Status |
|---|----------|---------|--------|
| A1 | High | `/ingest` and `/fleet/*` (drain, undrain, backend edits, route-hint, sniffer upload/delete) shared one `--auth-token`, so every edge `wayhouse` that pushes telemetry held a credential that could also drive every other instance (#102). | Fixed: `--ingest-token` unlocks only `POST /ingest`, `--auth-token` gates `/fleet/*`, and the aggregator refuses to start with `--auth-token` but no distinct `--ingest-token`. |
| A2 | High | The fan-out called each instance's self-reported `admin_url` with the fleet-wide `--instance-token`, so a pusher could name a host it controls and collect that token on the next operator call (#102). | Fixed: the URL is validated at ingest (own source address by default, or `--instance-url-allow`); a refused push is a `400`. The fan-out client no longer follows redirects. |
| A3 | Medium | Decoded path parameters (`pool`, `addr`, sniffer `name`) were pasted into the instance admin URL, so an encoded `..%2F` or `%3F` made one verb call another admin path with the instance token (#111). | Fixed: segments are validated (no empty, `.`, `..`, `/`, `\`, `?`, `#`) and pushed through `Url::path_segments_mut`; the sniffer name goes through `query_pairs_mut`. |

Residual: a holder of the aggregator's `--auth-token` can still drive the whole
fleet (one shared token, no per-verb roles), and `--ingest-token` is shared by
every pusher, so one compromised edge can overwrite another instance's reported
state (a pushed `instance` name is not bound to a credential).

## Checked and fine

- **TLS** (`crates/wayhouse-http/src/tls.rs`): rustls only, no custom verifiers,
  no `danger_accept_invalid_certs`; `--ca-file` adds roots on top of the
  Mozilla set instead of replacing them. Cert/key are hot-reloaded.
- **PROXY protocol**: the proxy only *writes* v1/v2 headers
  (`crates/wayhouse-core/src/proxy_protocol.rs`); it never parses untrusted ones,
  so there is no inbound parser to attack.
- **HTTP bodies**: admin/controller handlers use axum's `Json` extractor,
  which enforces axum's default 2 MiB body limit. The `to_bytes(.., usize::MAX)`
  calls are all in tests.
- **Config parsing**: YAML from a local file or the authenticated
  controller, `deny_unknown_fields`, validated into a resolved `Config`;
  cargo-fuzz harnesses exist for `parse_config`, `extract_sni` and route
  matching.
- **WASM sniffers** (`crates/wayhouse/src/sniffer_loader.rs`): wasmtime
  with epoch-interruption timeouts and a per-call `StoreLimits` memory cap.
- **`unsafe`**: only in the `crates/plugins/*` guest crates (the wasm ABI's
  `alloc` and raw input slices), which run inside the wasmtime sandbox. None
  in the host binaries.
- **Session ids**: 256 bits from `thread_rng`; cookie is `HttpOnly`,
  `SameSite=Lax`, and `Secure` when the UI serves HTTPS itself.
- **UI passwords** (`--users-file`): Argon2 PHC hashes.
- **Tunnel private key**: written with mode `0600`
  (`crates/wayhouse/src/tunnel_client.rs:48`).
- **Address book**: refuses loopback, multicast, broadcast and unspecified
  addresses (`crates/wayhouse-controller/src/addresses.rs:362`).
- **CI**: no `pull_request_target`, no `secrets.*`, untrusted refs passed to
  scripts via `env:` rather than inline `${{ }}` interpolation.
- **Secrets in history**: scanned on 2026-10-03 (gitleaks + trufflehog),
  clean apart from the intentional test fixtures.
