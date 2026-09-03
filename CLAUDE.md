# CLAUDE.md

Guidance for Claude Code (and any AI agent) working in this repository.
Humans: this doubles as the contributor quick-reference.

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
docs/                       the plan (00–09) — source of truth for design
crates/
  gsp-config/               YAML config: raw types, validation, resolved `Config`
  gsp-core/                 data plane
    snapshot.rs            immutable `Snapshot` (listeners + pools) behind ArcSwap
    pool.rs                 `Pool`, `Backend` (health + active count), `BackendGuard`
    listener.rs             TCP accept loop (one task per worker, SO_REUSEPORT)
    listener_udp.rs         UDP recv loop + worker-local session table + reply pump
    proxy.rs                per-connection byte pump (+ PROXY protocol header write)
    proxy_protocol.rs       PROXY protocol v1/v2 header encoder (write-only)
    sniff.rs                sniffer API seam (trait + registry) — no built-in sniffers; game protocol parsing loads as plugins (Phase 9)
    resolver.rs             external resolver seam: trait Resolver + resolve_pool (the async route walk); transports live in gsp
    route_hint.rs           push-resolver src_ip→pool table (POST /route-hint), lock-free read
    health.rs               active health-check sweep task (tcp_connect + udp_probe)
    ratelimit.rs            per-listener token-bucket rate limiter (src_ip + /24 / /64)
    limits.rs               process-wide caps (max_connections / max_udp_sessions / new-session rate)
    geo.rs                  optional MaxMind GeoIP country lookup (GeoDb) for the geo filter
    runtime.rs              owns listener + health tasks + route-hint table, holds the ArcSwap
    net.rs                  socket helpers (SO_REUSEPORT bind, IP_PKTINFO, IP_FREEBIND, IP_TRANSPARENT / TPROXY)
    metrics_defs.rs         canonical metric names — ALL metric names live here
    util.rs                 tiny helpers (monotonic now_ms)
  gsp/                       binary
    main.rs                 CLI, tracing, runtime bring-up, shutdown
    admin.rs                axum admin API: GET /healthz /readyz /metrics /pools, POST /route-hint
    resolver.rs             HttpResolver (reqwest) + build_resolvers(&Config)
    reload.rs               SIGHUP + file-watch → rebuild snapshot → atomic swap
```

Dependency direction: `gsp` → `gsp-core` → `gsp-config`. Never reverse it.

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
| Run | `cargo run -p gsp -- --config config.example.yaml` |
| Validate a config | `cargo run -p gsp -- --config <file> --check` |
| Reload a running proxy | edit the config file, or `kill -HUP <pid>` |

`GSP_LOG` sets the log filter (`GSP_LOG=debug`, `GSP_LOG=gsp_core::health=trace`, …).

---

## Guardrails

### Hard rules — do not break these

1. **Run `make check` before every commit.** CI fails on `cargo fmt` diffs and on any
   clippy warning (`-D warnings`). No exceptions, no `#[allow]` to silence a real lint
   without a one-line justification comment.
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
7. **Crate boundaries:** `gsp-config` depends only on `serde` + `thiserror`.
   `gsp-core` has no HTTP / CLI / `axum` / `reqwest` dependency — that belongs to
   `gsp`. External resolvers follow the same seam as sniffers: the `Resolver`
   trait lives in `gsp-core`, the HTTP/gRPC clients in `gsp`.
8. **Commit only when the user asks.** If not on a feature branch, branch off `main`
   first. End commit messages with the `Co-Authored-By:` and `Claude-Session:`
   trailers. **Do not `git push`** — credentials are not available in this environment
   unless the user has set them up.

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
- Keep comments at the density of the surrounding code. Module-level `//!` docs should
  say what the module is for and note deliberate simplifications.

### When you touch X, also touch Y

| Change | Also update |
|--------|-------------|
| Config schema | `gsp-config` raw+resolved types, `validate()`, `config.example.yaml`, `docs/05` |
| New metric | `metrics_defs.rs`, `docs/06` |
| New routing matcher / balancer | `docs/03`, `config.example.yaml`, tests |
| New / changed sniffer seam | `gsp_core::sniff`, `docs/03`, `docs/08` (Phase 9). NB: no game sniffers are compiled in — they load as plugins (Phase 9), never as core code or a fork. |
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

Phase 3 (routing intelligence) shipped: a priority-ordered
`routes:` list with `always` / `client_cidr` / `dst` / `port` / `first_bytes`
(`prefix` + `length`) / `sni` matchers, the `consistent_hash` balancer
(rendezvous hash, pool `hash_on`), the UDP `prefix:` listener (one wildcard
`IP_PKTINFO` socket per routed prefix, via `nix` — still zero `unsafe`) + TCP
`freebind:`, the sniffer API **seam** (`gsp_core::sniff` — trait + `sniffer`
matcher, **no built-in sniffers**; the loader is Phase 9), and the
`POST /route-hint` push resolver (per-listener `route_hint: true`).

Phase 4 (external resolver): HTTP + gRPC transports,
`pool` + `target` results, `on_error` (`reject`/`fallback_route`/`stale_ok`),
TTL'd LRU cache. Phase 5 (operability): connection draining with a grace period,
runtime listener add/remove/rebind, `draining` / `disabled` backend states, full
CRUD admin API. Deferred: `Resolution.sticky_key`, `GET /sessions`.

**Phases 0–6 done.** Phase 6 (client-IP preservation): per-pool
`proxy_protocol: none | v1 | v2 | v2-udp` writes a PROXY protocol header to the
upstream TCP connection (v1/v2) or the first datagram of each UDP session
(v2-udp); and `transparent: true` on a TCP **or UDP** listener is Linux TPROXY —
`IP_TRANSPARENT` listen socket, original destination read per connection /
datagram, client-`ip:port`-bound upstream socket, and (UDP) a per-session
`IP_TRANSPARENT` reply socket bound to the original destination. `socket2` is on
0.6. Setup docs in `docs/04`.

**Phase 7 (security & hardening) — in progress.** Slice 1: per-listener
`allow` / `deny` CIDR filter chain (`gsp_config::Acl` on `ListenerConfig::acl`),
checked on the client source IP before routing (TCP accept + UDP first datagram);
`deny` wins, a non-empty `allow` is default-deny. Slice 2: per-listener
`rate_limit: { per_ip, per_net }` token bucket (`gsp_config::RateLimit` →
`gsp_core::ratelimit::RateLimiter`, one per listener shared across workers) on
new connections / new UDP sessions, checked after the ACL; `per_net` keyed by
/24 (v4) / /64 (v6). Slice 3: process-wide `settings.limits`
(`max_connections` / `max_udp_sessions` / `max_new_sessions_per_sec`, startup-only)
→ `gsp_core::limits::GlobalLimits` (atomic counters + a new-session token bucket,
RAII `LimitGuard`), refused before allocation. Blocked ⇒ silent drop +
`gsp_filter_blocked_total{listener,filter="acl"|"rate_ip"|"rate_net"|"max_conn"|"max_udp"|"max_new_rate"}`.
Slice 4: UDP `first_packet_gate: true` — a session opens only when the first
datagram is positively recognised (non-`reject` sniffer hint or a matching
`first_bytes` route: `ListenerConfig::first_packet_recognised`), checked before
the `route_hint` lookup; else `gsp_datagrams_dropped_total{reason="first_packet_gate"}`.
Slice 5: automated amplifier-checklist tests
(`crates/gsp-core/tests/amplification.rs`; `docs/07` checklist ticked). Slice 6:
`gsp_config::CidrSet` radix trie backs `Acl::permits`. Slice 7: optional GeoIP
filter — `settings.geo_db` + per-listener `geo: { allow, deny }` (ISO codes),
`gsp_config::GeoAcl` decision + `gsp_core::geo::GeoDb` (dep `maxminddb`,
loaded once via `Runtime::start_with_geo`), checked after the CIDR ACL, fails
closed. Still to do: parser fuzzing, NFR load tests. See `HANDOVER.md` and
`docs/08`. Don't half-land a slice.
