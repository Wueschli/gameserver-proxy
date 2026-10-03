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
| F1 | High | Bearer tokens and the UI password were compared with `==`, which returns at the first mismatching byte. An attacker who can reach any authenticated API can, in principle, recover the token byte by byte from response timing. All ten sites now use `gsp_http::token_eq`, a length-checked constant-time comparison (unit-tested). | `crates/gsp/src/admin.rs:86`, `crates/gsp-controller/src/{auth.rs:34, intent/api.rs:100, peers/api.rs:150, proxy_peers/api.rs:142, addresses/api.rs:57, adopt.rs:80, ha/routes.rs:42}`, `crates/gsp-aggregator/src/auth.rs:35`, `crates/gsp-ui/src/api.rs:217`; helper in `crates/gsp-http/src/lib.rs` |
| F2 | Medium | The CI workflow had no `permissions:` block, so every job got the repository's default `GITHUB_TOKEN` scope. No job writes anything, so the workflow now declares `permissions: contents: read` at the top level. (Fork PRs already get a read-only token; this mainly protects `push`/`schedule`/`workflow_dispatch` runs from a compromised action or dependency.) | `.github/workflows/ci.yml:36` |

Timing attacks over a network are noisy and need many samples, so F1 is
"High" on impact (full admin takeover) rather than on ease. It is cheap to
fix and every auth path shared the same pattern.

## Open findings (not fixed here)

| # | Severity | Finding | Where | Suggested fix |
|---|----------|---------|-------|---------------|
| O1 | Medium | Every API is **open when no token is configured** (`gsp` admin, controller, aggregator, UI without `--ui-password`/`--users-file`). Defaults bind loopback (`127.0.0.1:990x`), which is safe, but nothing warns when an operator binds a non-loopback address without a token. | `crates/gsp-config/src/lib.rs:227`, `crates/gsp-controller/src/auth.rs:24`, `crates/gsp-aggregator/src/auth.rs:25`, `crates/gsp-ui/src/api.rs:193` | Log a `warn!` at startup (or refuse, behind an `--insecure-no-auth` flag) when the listen address is not loopback and no token/password is set. |
| O2 | Medium | UI sessions **never expire** and the store is unbounded. A stolen cookie is valid until the process restarts; in open mode anyone can grow the map with `POST /ui/login`. | `crates/gsp-ui/src/session.rs:31-72` | Store a creation/last-seen time, expire after an idle and an absolute TTL, cap the map size, add `Max-Age` to the cookie. |
| O3 | Medium | No rate limit on `POST /ui/login`. With `--users-file` each attempt runs Argon2 (CPU-heavy), so login is both a brute-force target and a cheap CPU DoS. | `crates/gsp-ui/src/api.rs:196`, `crates/gsp-ui/src/users.rs:66` | Per-IP token bucket on the login route (the data plane already has `ratelimit.rs` to borrow from); bound concurrent Argon2 verifications with a semaphore. |
| O4 | Medium | Gossip datagrams are HMAC-authenticated but carry **no freshness** (timestamp/nonce), so an on-path attacker can replay a captured health broadcast. LWW versioning limits the effect to re-asserting already-seen states, but a replayed "down" can briefly flap a backend. | `crates/gsp-core/src/gossip.rs:362-380` | Include a sender timestamp in the MAC'd payload and drop datagrams older than a window. |
| O5 | Low | The SSE config-subscribe clients buffer until `\n\n` with no size cap, so a malicious or broken controller can grow memory without bound. The controller is a trusted peer (pinned URL, optional TLS + `--ca-file`), hence Low. | `crates/gsp/src/controller_client.rs:149-168`, similar in `crates/gsp-agent/src/proxy_subscribe.rs` | Cap the buffer (e.g. 16 MiB) and reconnect when exceeded. |
| O6 | Low | Third-party actions are referenced by mutable tag (`dtolnay/rust-toolchain@stable/@nightly`, `Swatinem/rust-cache@v2`, `actions/*@v4/v5`). `taiki-e/install-action` and the Trivy image are already pinned by SHA/digest. With F2 the blast radius is read-only, but a hijacked tag could still exfiltrate cache contents. | `.github/workflows/ci.yml` (see `uses:` lines) | Pin to commit SHAs with a version comment; let Dependabot bump them. |
| O7 | Low | A short or guessable token is accepted as-is; there is no minimum length or entropy check on `--auth-token`, `--ha-token`, `settings.admin.auth_token`, or the gossip PSK. | CLI args in each `main.rs`, `crates/gsp-config/src/lib.rs:201` | Reject (or warn on) tokens/PSKs shorter than ~32 chars at startup. |

## Checked and fine

- **TLS** (`crates/gsp-http/src/tls.rs`): rustls only, no custom verifiers,
  no `danger_accept_invalid_certs`; `--ca-file` adds roots on top of the
  Mozilla set instead of replacing them. Cert/key are hot-reloaded.
- **PROXY protocol**: the proxy only *writes* v1/v2 headers
  (`crates/gsp-core/src/proxy_protocol.rs`); it never parses untrusted ones,
  so there is no inbound parser to attack.
- **HTTP bodies**: admin/controller handlers use axum's `Json` extractor,
  which enforces axum's default 2 MiB body limit. The `to_bytes(.., usize::MAX)`
  calls are all in tests.
- **Config parsing**: YAML from a local file or the authenticated
  controller, `deny_unknown_fields`, validated into a resolved `Config`;
  cargo-fuzz harnesses exist for `parse_config`, `extract_sni` and route
  matching.
- **WASM sniffer plugins** (`crates/gsp/src/sniffer_loader.rs`): wasmtime
  with epoch-interruption timeouts and a per-call `StoreLimits` memory cap.
- **`unsafe`**: only in the `crates/plugins/*` guest crates (the wasm ABI's
  `alloc` and raw input slices), which run inside the wasmtime sandbox. None
  in the host binaries.
- **Session ids**: 256 bits from `thread_rng`; cookie is `HttpOnly`,
  `SameSite=Lax`, and `Secure` when the UI serves HTTPS itself.
- **UI passwords** (`--users-file`): Argon2 PHC hashes.
- **Tunnel private key**: written with mode `0600`
  (`crates/gsp/src/tunnel_client.rs:48`).
- **Address book**: refuses loopback, multicast, broadcast and unspecified
  addresses (`crates/gsp-controller/src/addresses.rs:362`).
- **CI**: no `pull_request_target`, no `secrets.*`, untrusted refs passed to
  scripts via `env:` rather than inline `${{ }}` interpolation.
- **Secrets in history**: scanned on 2026-10-03 (gitleaks + trufflehog),
  clean apart from the intentional test fixtures.
