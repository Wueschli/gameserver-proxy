# AGENTS.md

Guidance for any AI coding agent working in this repository, following the
[agents.md](https://agents.md) convention.
Humans: this doubles as the contributor quick-reference.

Tool-specific entry points (e.g. `CLAUDE.md`) are thin pointers to this file — keep
the guidance here, not in them.

---

## Start here (every session)

1. Read [`HANDOVER.md`](HANDOVER.md) — current state, locked decisions, and the plan
   for the next slice.
2. Run `make check` once to confirm a green baseline (fmt + clippy `-D warnings` +
   tests) before changing anything.
3. Skim the relevant `docs/` chapter before any structural change (design is the
   source of truth there, not the code).

---

## What this is

A **game-agnostic game server reverse proxy**: one entry point in front of arbitrary
game servers, forwarding TCP (and later UDP) transparently to backend pools without
knowing the game protocol. Written in Rust on `tokio`.

The full design lives in [`docs/`](docs/) — read [`docs/00-overview.md`](docs/00-overview.md)
and [`docs/02-architecture.md`](docs/02-architecture.md) before making structural
changes. Current progress and the next step are in [`HANDOVER.md`](HANDOVER.md).

---

## Repository layout

```
Cargo.toml                  workspace (resolver 2, edition 2021)
rust-toolchain.toml         pins stable
config.example.yaml         reduced v0 config schema
Makefile                    make check / test / run / fmt / lint
docs/                       the plan (00–12) — source of truth for design
deploy/                     reference Dockerfile (5 targets), compose demo, k8s manifests, smoke.sh (docs/12)
crates/
  gsp-config/               YAML config: raw types, validation, resolved `Config`
    src/                     lib.rs (entry points + re-exports), schema.rs (raw YAML types), validate.rs (`validate()`; tests in validate/tests.rs), parse.rs (value parsers), resolved.rs (validated `Config` & friends), cidr.rs (`Cidr`/`CidrSet`/`Acl`/`GeoAcl`), matcher.rs (`Matcher`, `extract_sni`), keys.rs (`base64_decode_32`)
    fuzz/                    cargo-fuzz harnesses (extract_sni / route_match / parse_config) — standalone workspace
  gsp-core/                 data plane
    snapshot.rs            immutable `Snapshot` (listeners + pools + sources + resolvers) behind ArcSwap
    pool.rs                 `Pool`, `Backend` (health + active count + AdminState), `BackendGuard`
    listener.rs             TCP accept loop (one task per worker, SO_REUSEPORT)
    listener_udp.rs         UDP recv loop + worker-local session table + reply pump
    listeners.rs            `ListenerManager` — runtime listener add/remove/rebind, reconciled by name on reload
    proxy.rs                per-connection byte pump (+ PROXY protocol header write)
    proxy_protocol.rs       PROXY protocol v1/v2 header encoder (write-only)
    sniff.rs                sniffer API seam (trait + ArcSwap-backed registry) — no built-in sniffers; game protocol parsing loads as plugins (Phase 9)
    resolver.rs             external resolver seam: trait Resolver + resolve_route (the async route walk), ArcSwap-backed registry; transports live in gsp
    discovery.rs            `BackendSource` seam + `Discovery` last-known-good cache + refresh_loop (Phase 8)
    sources.rs              `SourceManager` — runtime `backend_sources:` reconcile on reload (discovery analogue of ListenerManager)
    route_hint.rs           push-resolver src_ip→pool table (POST /route-hint), lock-free read
    health.rs               active health-check sweep task (tcp_connect + udp_probe); phase 13: publishes each result into the gossip fabric and refreshes each backend's domain_down from it
    gossip.rs               Tier-2 regional health fabric (phase 13, docs/10 "Tier 2"): embedded `foca` SWIM mesh + HMAC-authenticated UDP transport + a per-backend LWW health broadcast, spawned only when `settings.gossip` is set
    ratelimit.rs            per-listener token-bucket rate limiter (src_ip + /24 / /64)
    src_conns.rs            per-listener concurrent per-source connection / session cap
    limits.rs               process-wide caps (max_connections / max_udp_sessions / new-session rate)
    geo.rs                  optional MaxMind GeoIP country lookup (GeoDb) for the geo filter
    error.rs                typed errors for the public API: `ProxyError`, `ListenerError`, `SourceError` (no `anyhow` in `gsp-core`)
    drain.rs                `ConnTracker`/`ConnGuard` — live connection/session count + registry for graceful shutdown and GET /sessions
    overlay.rs              `BackendOverlay` — runtime POST/DELETE backend edits, layered on the file config
    runtime.rs              owns listener + health + source-manager tasks + route-hint table, holds the ArcSwap
    net.rs                  socket helpers (SO_REUSEPORT bind, IP_PKTINFO, IP_FREEBIND, IP_TRANSPARENT / TPROXY)
    metrics_defs.rs         canonical metric names — ALL metric names live here
    util.rs                 tiny helpers (monotonic now_ms)
  gsp/                       binary
    main.rs                 CLI, tracing, runtime bring-up, shutdown
    admin.rs                axum admin API: GET /healthz /readyz /metrics /pools /config /sessions, POST /route-hint /admin/drain /admin/undrain, PATCH+POST+DELETE backend routes
    resolver.rs             HttpResolver (reqwest) + GrpcResolver (tonic) + build_resolvers(&Config)
    discovery.rs            ConsulSource / KubernetesSource adapters (Phase 8) + TunnelSource (phase 14 slice 5, `docs/11`) — resolves a pool's backends from gsp-controller's backend-peers registry, pinned to a configured pubkey
    dns_srv.rs              DnsSrvSource (Phase 8) — behind the `dns-srv` feature (`hickory-resolver`), `dns_srv_disabled.rs` stands in without it
    grpc_resolver.rs        GrpcResolver (tonic) — behind the `grpc-resolver` feature (`tonic`/`prost`, and `protoc` in `build.rs`), `grpc_resolver_disabled.rs` stands in without it
    sniffer_loader.rs       WasmSniffer + SnifferLoader — the wasmtime-based sniffer plugin loader (Phase 9); behind the `wasm-sniffers` feature, `sniffer_loader_disabled.rs` stands in without it
    procinfo.rs             gsp_fd_open / gsp_fd_limit — fd sampling (gsp_build_info lives in gsp_http::metrics)
    reload.rs               SIGHUP + file-watch + admin-triggered reload → rebuild snapshot → atomic swap
    controller_client.rs    `--controller <url>` config source (phase 10+11): initial GET /config + a GET /config/subscribe (SSE) client, reconnect w/ backoff, feeds reload::apply_config
    tunnel_client.rs        `--tunnel-*` (phase 14 slice 4, `docs/11`): brings up this proxy's shared WireGuard interface and subscribes to `gsp-controller`'s backend-peers registry, reconciling every registered origin onto the interface's peer list
    tunnel_address.rs       `--tunnel-*` address resolution: registers for an address before the interface comes up, falls back to the saved `<key-file>.address`
    proxy_register.rs       `--tunnel-*` (phase 14 slice 7, `docs/11`): registers this proxy's own pubkey + public endpoint with `gsp-controller`'s proxy-peers registry, so every origin's `gsp-agent` can peer with it without origin-side reconfiguration
  gsp-controller/            binary — Tier-1 config distribution (phases 10–14, docs/10 "The controller" + docs/11: `standalone`/`slave` roles, Raft HA, canary rollout, backend/proxy-peers registries)
    store.rs                `Store` — embedded sled KV (ADR 20): revisions + current-pointer trees, catch-up range scan
    api.rs                  POST/GET /config, GET /config/subscribe (SSE), GET /config/revisions(+/{rev}(/diff)), POST /config/rollback/{rev}
    peers.rs                 backend-peers registry (phase 14 slice 2, `docs/11`): POST/GET /peers(+/{name}), GET /peers/subscribe (SSE) — origins register here, proxies subscribe
    addresses.rs            tunnel address book: allocation, pinning, release; shared by both peer registries
    lease.rs                `--tunnel-lease-ttl` sweeper: expires owners unseen for the TTL through each registry's `expire` (a replicated `Expire` entry under HA)
    addresses/api.rs        `GET /tunnel/addresses` and the release plumbing over the address book
    proxy_peers.rs           proxy-peers registry (phase 14 slice 7, `docs/11`) — the mirror image of `peers.rs`: proxies register here, origins' `gsp-agent`s subscribe
    registry.rs             the registry core both `peers.rs` and `proxy_peers.rs` run on: `Registration` trait, `RegistryState` (log + `current` tree in one transaction, Raft-index-idempotent `register_applied`/`remove_applied`), handlers, subscribe/catch-up
    registry/ha.rs          the registries' write path under HA: the unchanged check (`normalize`/`is_unchanged`, background `Touch`), `Register*`/`Release` through `ha::client::propose_write`, the network-mismatch and not-initialized `503`s
    ha/cluster_state.rs     the cluster's recorded tunnel network + `initialized` marker (replicated; registry entries apply against it, never a node's flag)
    ha/apply_registry.rs    applying `Register*`/`Release`/`Touch` entries into the registries and the address book, crash-idempotent per database
    ha/init.rs              leader-side initialization: asks every voter what pre-HA data it holds (`choose_source`), proposes `SetTunnelNetwork` or `Import`; `--ha-import-source`
    ha/import.rs            pre-HA data on upgrade: set-aside (`*.pre-ha`), `ImportContent`, `GET /raft/pre-ha`, the once-only `Import` apply
    ha/members.rs           `/admin/ha/members` (list, add, remove, re-address) with the `/raft/whoami` identity check; `--ha-join` nodes are added here
  gsp-aggregator/            binary — fleet read/operational-verb path (phase 10+11, docs/10 "The aggregator"); no gsp-core/gsp-config dependency, stays decoupled from the data-plane crates
    ingest.rs               `IngestStore` — in-memory, latest-write-wins per-instance map (deliberately unpersisted); `IngestPayload` (pool/backend summary + session counts, self-reported `admin_url`)
    api.rs                  POST /ingest, GET /fleet/pools|sessions|healthz|subscribe (SSE)
    fanout.rs               intent-verb fan-out to instance admin APIs — targeted (drain/undrain) + broadcast (backend add/patch/delete, route-hint)
  gsp-ui/                    binary — the admin GUI's BFF (phase 10+11, docs/10 "The admin GUI"); dedicated process, not hosted in the controller or aggregator; holds neither's authority, no gsp-core/gsp-config dependency
    session.rs              `SessionStore` — in-memory random session ids (ephemeral, like the aggregator's store)
    api.rs                  POST /ui/login|logout, GET /ui/session; merges aggregator_proxy/controller_proxy/ws into the session-gated route group
    auth.rs                 session-cookie gate (`require_session`) — distinct from the controller's/aggregator's bearer-token gates; the browser never holds a bearer token
    aggregator_proxy.rs     proxies fleet reads + phase-9 operational verbs to gsp-aggregator (`--aggregator-url`/`--aggregator-token`)
    controller_proxy.rs     proxies gsp-controller's config API (submit, revisions, diff, rollback) its `GET /tunnel/addresses`, and the registry `DELETE`s that release an address, to gsp-controller (`--controller-url`/`--controller-token`)
    proxy_util.rs           shared `forwardable_headers` — both proxies forward the upstream response's headers, not just status+body
    fleet_feed.rs           single shared subscription to the aggregator's `/fleet/subscribe` SSE feed, fanned out via a broadcast channel
    ws.rs                   GET /ws/fleet — the browser's live-updates WebSocket, fed by fleet_feed
    web/                    standalone npm project (own package.json, never a Cargo workspace member) — the React + Vite + TS frontend; `make ui` builds it to `dist/`, served by `--static-dir`
  gsp-agent/                 binary — phase 14 (backend transport, `docs/11`) origin-side agent; brings up a local WireGuard interface (`defguard/wireguard-rs`: kernel primary, `boringtun` userspace fallback) and registers its pubkey + fronted backend addresses with `gsp-controller`'s backend-peers registry
    keypair.rs              persists this origin's WireGuard private key across restarts — a stable identity, never regenerated
    interface.rs            `bring_up_with` — creates/configures the interface, kernel backend first unless `--userspace` forces boringtun
    address_store.rs        persists the controller-assigned tunnel address next to the key (start-from-saved while the controller is down)
    register.rs             `POST /peers` client — registers once, then re-registers on a fixed interval
    proxy_subscribe.rs      phase 14 slice 7 (`docs/11`): subscribes to `gsp-controller`'s proxy-peers registry and reconciles every registered proxy onto this origin's interface — the mirror image of `gsp`'s `tunnel_client.rs`
  gsp-http/                  the one place production HTTP clients are built (`builder()`/`client()`, with `--ca-file`'s extra roots, set once from each binary's `main`) and, in `sse` (`EventBuffer`, which every `/…/subscribe` client reassembles events with: it keeps only the unterminated tail and refuses one past 16 MiB), and, in `server` (the one constant-time `--auth-token` bearer middleware, `require_bearer`, shared by every fleet HTTP server), in `metrics` (`install()` + the `GET /metrics` route the controller, aggregator and UI mount behind that gate) and `tls` (cargo feature `server`, which every serving binary enables), the native-TLS server side (`TlsListener` for `axum::serve`, hot-reloading `ReloadingCert`, `TlsArgs` + `serve`); test-only CA/cert fixtures in `tests/fixtures/`
  gsp-bench/                 latency / load harness vs. NFR N1/N2 (`make bench`)
  gsp-fleet-tests/            phase 10+11 slice 12 integration tests — spawns real
                              gsp/gsp-controller/gsp-aggregator/gsp-ui binaries as
                              child processes and drives them over real HTTP
                              (`cargo test -p gsp-fleet-tests`, included in `make check`); phase 14's
                              `tests/tunnel.rs` is `#[ignore]`d and runs via `make tunnel-e2e`
  plugins/                   first-party sniffer plugins (a2s/minecraft/quic/wireguard/regex-firstbytes) + gsp-sniffer-abi — standalone workspace, `make plugins`
```

Dependency direction: `gsp` → `gsp-core` → `gsp-config` (`gsp-bench` → `gsp-core`
too, tools only). Never reverse it.

---

## Commands

The toolchain is installed via `rustup`. If `cargo` is not on `PATH`:

```sh
export PATH="$HOME/.cargo/bin:$PATH"     # or: source "$HOME/.cargo/env"
```

`protoc` must be on `PATH` — `crates/gsp/build.rs` generates the gRPC resolver
client from `crates/gsp/proto/resolver.proto`.

| Task | Command |
|------|---------|
| Everything CI runs | `make check` (fmt check + clippy `-D warnings` + tests) |
| Format | `make fmt` (writes) / `cargo fmt --all --check` (verify) |
| Lint | `cargo clippy --all-targets -- -D warnings` |
| Test | `cargo test --all` |
| Minimal edge build | `make test-minimal` (`gsp` with `--no-default-features`: clippy + tests; also in `make check` and the `test` CI job). Optional `gsp` cargo features, issue #62: `wasm-sniffers` (wasmtime), `grpc-resolver` (tonic/prost, `protoc`), `dns-srv` (hickory). Build it with `cargo build --release -p gsp --no-default-features` |
| Tunnel e2e | `make tunnel-e2e` (rootless; needs `unshare`, `ip`, `nsenter`; `TUNNEL_BACKEND=kernel\|userspace`, default kernel; also the `tunnel` CI job) |
| Deploy images | `make deploy-images` (needs Docker; builds the five `deploy/Dockerfile` targets and runs `--version` on each; `BIN_SOURCE=prebuilt` uses binaries from `deploy/prebuilt/`) |
| Deploy scan | `make deploy-scan` (after `deploy-images`; needs Docker + `trivy`): Trivy over the five images (OS packages, embedded Rust crates, secrets) and `Cargo.lock` / the UI's `package-lock.json`, HIGH/CRITICAL with a fix; exits 1 on findings, reports in `target/trivy/`. In CI it is the separate, **informational** `trivy` job after `deploy` (non-blocking; scans `deploy`'s images from a `docker save` artifact; run summary + warnings + `trivy-reports` artifact; the official `aquasec/trivy` image pinned by digest, not `trivy-action`). Rust coverage is GHSA only — `cargo audit` (the `audit` CI job, `make audit`) covers RustSec. Accepted findings: `.trivyignore` |
| Deploy smoke | `make deploy-smoke` (needs Docker; compose demo + `deploy/smoke.sh`; also the `deploy` CI job) |
| Tunnel e2e (nextest) | `make tunnel-e2e-ci` (needs `cargo install cargo-nextest --locked`; writes `target/nextest/ci/junit.xml`; what the CI `tunnel` job runs) |
| Audit | `make audit` (needs `cargo install cargo-audit --locked`): `cargo audit` over the root, plugins and fuzz lockfiles; exits 1 on a vulnerability, JSON in `target/cargo-audit/`. In CI the **informational** `audit` job (every push/PR, plus nightly; run summary + warnings + `cargo-audit` artifact). Accepted advisories go in `.cargo/audit.toml` (none yet, so the file does not exist) |
| Fuzz | `make fuzz` (needs `rustup toolchain install nightly` + `cargo install cargo-fuzz`; see `crates/gsp-config/fuzz/README.md`) |
| Bench | `make bench` (latency / load harness vs. NFR N1/N2; see `crates/gsp-bench/README.md`) |
| Sniffer plugins | `make plugins` (needs `rustup target add wasm32-unknown-unknown`; builds `crates/plugins/` to `wasm32-unknown-unknown`; see `crates/plugins/README.md`) |
| gsp-ui frontend | `make ui` (needs Node/npm; builds `crates/gsp-ui/web/` to `dist/`, served by `gsp-ui --static-dir`; see `crates/gsp-ui/web/README.md`) |
| gsp-ui frontend tests | `make ui-test` (vitest) and `make ui-e2e` (Playwright, backend stubbed; both in the `ui` CI job) |
| Run | `cargo run -p gsp -- --config config.example.yaml` |
| Validate a config | `cargo run -p gsp -- --config <file> --check` |
| Reload a running proxy | edit the config file, or `kill -HUP <pid>` |

`GSP_LOG` sets the log filter (`GSP_LOG=debug`, `GSP_LOG=gsp_core::health=trace`, …).

---

## Guardrails

### Hard rules — do not break these

1. **Run `make check` before every commit.** CI fails on `cargo fmt` diffs and on any
   clippy warning (`-D warnings`). No exceptions, no `#[allow]` to silence a real lint
   without a one-line justification comment. Run `cargo fmt --all` (the writing form)
   as its own step immediately before that `make check` / commit — a fmt pass from
   earlier in the session does not cover edits made after it, and `--check` only
   reports diffs, it never fixes them. See HANDOVER.md "Workflow gotcha" for a case
   where skipping this let a `cargo fmt --all --check` failure reach CI.
   The curated clippy lints live in the root `[workspace.lints.clippy]`; a new crate
   opts in with `[lints] workspace = true`.
2. **No `unsafe`.** The codebase currently has zero. `socket2`/`nix` give safe
   wrappers for the syscalls we need. If `unsafe` ever becomes genuinely necessary, it
   needs a `// SAFETY:` comment *and* a note in `HANDOVER.md`.
3. **The `Snapshot` is immutable after `Snapshot::build`.** Never mutate it in place.
   A config/backend change = build a new `Snapshot` and `ArcSwap::store` it. Only
   `reload.rs` writes the swap; everything else reads.
4. **Nothing on the hot path may block or lock.** Hot path = per-connection accept,
   per-connection pump, and (phase 2) per-datagram handling. No blocking syscalls, no
   `std::sync::Mutex` held across `.await`, no per-iteration heap allocation in the
   copy loop. `Backend::observe` takes a short `Mutex` — that is deliberate and only
   runs on connect success/failure, not per byte. Keep it that way.
5. **Control plane may block; keep it off the data plane's threads conceptually.**
   Health checks, reload, discovery, admin API — these can do slow things. They must
   never stall accept/pump.
6. **All metric names go in `crates/gsp-core/src/metrics_defs.rs`** as `pub const`,
   and get documented in [`docs/06-operations-observability.md`](docs/06-operations-observability.md).
   Never inline a metric-name string literal at a call site.
7. **Crate boundaries:** `gsp-config` depends only on `serde` + `serde_norway` +
   `thiserror` + `base64` (WireGuard key validation).
   `gsp-core` has no HTTP / CLI / `axum` / `reqwest` dependency — that belongs to
   `gsp`. External resolvers follow the same seam as sniffers: the `Resolver`
   trait lives in `gsp-core`, the HTTP/gRPC clients in `gsp`.
   **Poisoned locks:** recover with `lock().unwrap_or_else(PoisonError::into_inner)`
   (same for `read`/`write`) — never `.unwrap()` / `.expect("…poisoned")`. The
   guarded state here is plain data, so one panicked task must not cascade into
   every later caller.
8. **Commit directly on `main` whenever it is useful** — a finished slice, a spec or
   plan, a green docs sweep; no need to ask first and no feature branch (owner's
   standing decision, 2026-10-01). Run `make check` first (rule 1). End commit
   messages with a `Co-Authored-By:` trailer identifying the agent that made the
   change, plus a session/trace trailer (e.g. `Agent-Session:`) when the tool
   provides one. **Do not `git push` unless the user asks** — pushing is outward-facing
   and is still the user's call.

### Soft rules — the intended way to work

- **Latency first.** Every design choice is weighed against added RTT (see NFR N1/N2
  in [`docs/01-requirements.md`](docs/01-requirements.md)). If you spawn a task or add
  a hop per connection, say so in the PR/commit and in `HANDOVER.md`.
- **Agnostic core.** Game-specific knowledge only ever lives in optional sniffer
  plugins or in an external resolver — never in `gsp-core`'s routing/forwarding paths.
- **Incremental, vertical slices.** Follow the phase plan in
  [`docs/08-roadmap.md`](docs/08-roadmap.md). Don't half-land a phase (e.g. don't add
  a UDP listener type without the session table). Update the roadmap's status legend
  when a phase item lands.
- **Tests with every behavioural change.** Unit tests in the module; end-to-end
  forwarding behaviour in `crates/gsp-core/tests/`. Integration tests may use
  `127.0.0.1:0` ephemeral sockets; no other network access.
- **Pre-1.0: prefer clean breaks over compatibility shims.** When you change the
  config schema, update *all four*: `gsp-config` types + `validate()`,
  `config.example.yaml`, and `docs/05-configuration.md`.
- **How sessions are run here (owner's expectations, 2026-10-02).** Larger work follows
  the superpowers flow: brainstorm (one question at a time, designs approved section by
  section) → a written spec in `docs/superpowers/specs/` → a written plan in
  `docs/superpowers/plans/` → execution with strict TDD (show the RED run, then GREEN) and
  verification before claiming done; big plans run subagent-driven (fresh implementer per
  task + spec/quality review + a final whole-branch review). Specs and plans are committed;
  `.superpowers/` is git-ignored scratch (ledgers, briefs, review packages). Reviewer
  findings that are deferred go into a GitHub issue, never silently dropped.
- **CI runs are slow** (the repo is public, so hosted minutes are free, but a full run
  takes ~10 min warm and ~22 min cold). Batch pushes, remember that docs-only pushes
  skip CI (`paths-ignore`), put `[skip ci]` in the tip commit message when a code push
  needs no run, and update `ROOTS` in `.github/scripts/changes.py` when adding a job.
  Never push without being asked (rule 8).
- Keep comments at the density of the surrounding code. Module-level `//!` docs should
  say what the module is for and note deliberate simplifications.

### When you touch X, also touch Y

| Change | Also update |
|--------|-------------|
| Config schema | `gsp-config` raw+resolved types, `validate()`, `config.example.yaml`, `docs/05` |
| New metric | `metrics_defs.rs`, `docs/06` |
| New routing matcher / balancer | `docs/03`, `config.example.yaml`, tests |
| New / changed sniffer seam | `gsp_core::sniff`, `docs/03`, `docs/08` (Phase 9). NB: no game sniffers are compiled in — they load as plugins (Phase 9), never as core code or a fork. |
| New optional `gsp` cargo feature | `crates/gsp/Cargo.toml` `[features]` (on by default), a `*_disabled.rs` stub that fails startup with a message naming the feature when the config needs it, a `--no-default-features` test, the AGENTS.md command table |
| Finished a roadmap item | status legend in `docs/08-roadmap.md`, `README.md` status block, `HANDOVER.md` |
| New per-connection task or hop | `HANDOVER.md` "latency ledger" note |
| Architectural decision | ADR table in `docs/09-technology-choices.md` |

---

## Architecture invariants

- **Data plane / control plane split.** Data plane reads an immutable `Snapshot` via
  `ArcSwap::load`; control plane builds new snapshots. No shared mutable config.
- **One accept task per (listener × worker)**, each with its own `SO_REUSEPORT`
  socket. Sessions are pinned to their accepting worker (matters for phase 2's
  thread-local UDP session table).
- **`BackendGuard` = one reserved session slot.** Held for the whole connection,
  released on `Drop`. It also carries passive health signals (`guard.observe(ok)`).
- **Health is advisory and eventually-consistent.** `healthy` is an `AtomicBool`;
  reads are lock-free. `rise`/`fall` thresholds live on the `Backend`.
- **Reload carries backend health across the swap by address.** New backends start
  optimistically healthy; the checker corrects within one interval.
- **No cross-instance shared state** (v1). Proxy instances are independent; HA is an
  anycast/L4-LB concern in front.

---

## Roadmap position

**All roadmap phases 0–14 are built**: the data plane (0–9, plus a
data-plane-completion pass and a perf pass — `splice(2)` TCP pump, `recvmmsg`
UDP ingress, timing-wheel idle expiry), the distributed control plane (10–13:
fleet aggregation, global config/intent store with hierarchy / HA / canary /
RBAC, regional health gossip — [`docs/10`](docs/10-distributed-control-plane.md))
and the WireGuard backend transport (14 — [`docs/11`](docs/11-backend-transport.md)).
No slice is in flight; remaining work is the open GitHub issues.

For what shipped, what's deferred, and per-feature latency cost, see
[`HANDOVER.md`](HANDOVER.md); for the full phase-by-phase plan and status,
[`docs/08-roadmap.md`](docs/08-roadmap.md); for locked architectural decisions,
the ADR table in [`docs/09-technology-choices.md`](docs/09-technology-choices.md).
