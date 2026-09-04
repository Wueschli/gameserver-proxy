# HANDOVER

State of the work, how to pick it up, and the traps.
Last updated: 2026-09-04.

Design is the source of truth in [`docs/`](docs/); locked decisions are the ADR
table in [`docs/09-technology-choices.md`](docs/09-technology-choices.md). This
file is the *current-state + gotchas* layer on top of that — per-phase
implementation narration lives in git history and `docs/08`, not here.

---

## Where the work is

**Roadmap phases 0–9 are complete**, plus two follow-on passes on `main`:

- **Data-plane completion** — ClientHello/first-bytes reassembly across TCP
  segments; `RouteHint.reject` hard drop; per-resolver `target` connect/idle
  timeouts; `weighted` balancer (pool `weights: { "ip:port": N }`); per-plugin
  sniffer config (`settings.sniffers.modules[].config` + the widened
  `sniff(in_ptr, in_len, cfg_ptr, cfg_len)` ABI — ADR 16a); live reload of
  `resolvers:` and `backend_sources:` (both registries `ArcSwap`-backed,
  reconciled by the reload task); `GET /sessions` live registry; first
  HTTP-level admin API tests.
- **Perf pass** — `splice(2)` zero-copy TCP pump (ADR 17); `recvmmsg(2)` UDP
  ingress batching (ADR 18); UDP idle expiry via a single-level timing wheel
  (ADR 19). Still deferred: `sendmmsg` UDP egress batching.
- **Listener port-range bind (F1.4)** — `bind: "host:lo-hi"` (e.g.
  `"0.0.0.0:30000-30999"`) spawns one real socket per port (× `workers`,
  `SO_REUSEPORT`-shared) under one listener config, sharing its
  routes/filters/pool selection; a route's `port` matcher still sees the real
  accepted/received port. `gsp_config::ListenerConfig::bind` is the primary
  (lowest) port, `extra_binds: Vec<SocketAddr>` the rest (empty for a plain
  bind); `ListenerConfig::binds()` iterates both. Capped at 1024 ports/range
  (`MAX_BIND_RANGE`); mutually exclusive with `prefix`. Was requirement F1.4
  in `docs/01-requirements.md`, written down at project start and never
  carried into a phase or deferred-work note until a documentation audit
  caught the gap — closed out right after, see `docs/08` Phase 3.
- **Build/fd metrics** — `gsp_build_info{version,commit}` (gauge, set once at
  startup; `commit` baked in by `crates/gsp/build.rs` via `git rev-parse`),
  `gsp_fd_open` (sampled every 5 s by `crates/gsp/src/procinfo.rs`, a small
  detached background task, `/proc/self/fd` on Linux) and `gsp_fd_limit`
  (`RLIMIT_NOFILE` soft limit via `nix::sys::resource::getrlimit`, sampled
  once — `nix` gained the `resource` feature, now also a direct `gsp`-binary
  dep, not just `gsp-core`'s). Lives in the binary, not `gsp-core` — host/ops
  observability, no data-plane seam needed. See `docs/06`.

**Next**: phases 10–12 (the distributed control plane —
[`docs/10-distributed-control-plane.md`](docs/10-distributed-control-plane.md)),
which are **design only, nothing built**. Or one of the polish items below, per
what the user wants.

**2026-09-04 design session**: `docs/10` gained a "Fleet topology" section —
the controller and the new aggregator are each a recursive tree of tiers (one
per failure domain, down to a single instance), not a single global service.
Control (Tier 1) pulls root→leaf with a static `standalone`/`slave` role per
tier (never inferred from connectivity — that would split-brain a partition);
the aggregator pushes leaf→root with a homogeneous schema at every hop
(chosen over pull specifically to avoid needing a network route down to every
proxy). Intra-tier HA (1 node vs. a Raft/etcd group) is an orthogonal setting
from the role. Adoption (flipping a `standalone` tier to `slave` post-install
via the admin UI) is noted but explicitly deferred.

**Locked scope for the first release** (`docs/08` phase 10+11, now merged
into one PoC-sized phase): one `standalone` controller + one aggregator,
`replicas: 1` each, no `slave` role, no HA, no adoption. Operator intent
(backend overlay/admin-state, route hints) **stays per-instance** as it is
today — only structural config moves into the controller for this release;
moving intent into the controller's revision log is phase 12. Tier-1 store is
an embedded `sled` KV, single node (ADR 20, `docs/09`) — the user explicitly
asked for a proper KV over pushing config files around. `docs/08`'s phase
10+11 section has the full 13-slice implementation plan (controller slices
1–5, aggregator slices 6–10, UI/tests/docs 11–13); phase 12 carries the
deferred hierarchy/HA/intent-migration work forward as its own phase.

**Admin GUI is served only by the root tier** (same session, added after the
above): a GUI per region would need per-region auth/RBAC kept consistent —
exactly what the hierarchy exists to avoid. Every `slave` tier stays a
machine-to-machine API, reachable directly as break-glass but never as a
served web frontend.

**Slice 1 done** (2026-09-04, new crate `crates/gsp-controller`, lib
`gsp_controller` + bin `gsp-controller`): `store::Store` wraps two `sled`
trees (`revisions`, `meta`) — `open`, `current_revision`, `get`, `current`,
`put` (assigns the next monotonic revision, writes it + the `current` pointer
in one `sled` transaction, then flushes). 4 unit tests incl. a
reopen-persists check. The binary opens the store and serves `GET /healthz`
only — `POST /config` (slice 2) and the subscribe/change-stream endpoint
(slice 3) aren't built yet. Workspace gained `sled` + `tempfile` (dev-dep) in
the root `Cargo.toml`. `make check` (fmt + clippy `-D warnings` + `cargo test
--all`) passes with the new crate in the workspace.

**Slice 2 done**: `crates/gsp-controller/src/api.rs` — `POST /config` runs
the same `gsp_config::parse_str` (parse + `validate()`) a proxy runs on a
file reload; a rejected submission (`422`, JSON error body) never touches
`Store::put`, so the current revision stays whatever it was — same
bad-reload-keeps-the-old-snapshot rule as the proxy, one hop earlier.
`GET /config` returns the current revision's raw text with an
`X-Config-Revision` header (`404` before any submission). Verified against
the real `config.example.yaml`. 3 new tests (7 total in the crate).
`gsp-config` is now a `gsp-controller` dependency; `tower` added as a
gsp-controller dev-dependency for the in-module `axum` router tests (mirrors
`gsp`'s own in-module admin HTTP tests).

**Slice 3 done**: `GET /config/subscribe?since=<revision>` (SSE) on
`gsp-controller` — catch-up range from `Store::revisions_after` then a live
tail off a `broadcast::Sender<u64>` `submit_config` feeds; a lagging
subscriber just re-runs the catch-up query, so `Store` (never forgets a
revision) is the only source of truth, no delivery state on the writer side.
5 new tests incl. a forced-lag one. `gsp` gained `--controller <url>`
(`conflicts_with` `--config`); `main.rs` now loads its first config (file or
`controller_client::fetch_current`'s `GET /config`) inside `block_on`, since
the controller path needs an async HTTP call before anything else exists.
`reload::apply` split into itself (file read) + a new `pub(crate)
apply_config` (validated-`Config` → rebuild/reconcile) so both the file
reload and `controller_client::run`'s pushed revisions share one pipeline.
`controller_client::run` holds the SSE connection, reconnects with capped
exponential backoff (500 ms → 30 s) from the last-*applied* cursor (not
where the connection started — a bug caught by the live smoke test below and
fixed: the cursor is now threaded through by `&mut` so a mid-stream error
doesn't roll it back and force a pointless replay). An invalid pushed
revision is logged, skipped, and still advances the cursor (must not replay
forever). Verified live end-to-end (not just unit tests): started
`gsp-controller`, submitted `config.example.yaml`, started
`gsp --controller <url>` and confirmed it came up and served `/healthz`,
pushed a second revision and watched `gsp` log
`configuration reloaded source=controller revision 2`, then killed the
controller and confirmed `gsp` kept running on its last config while
retrying with growing backoff (`cursor=2` correctly, post-fix). Workspace
gained `tokio-stream` (gsp-controller's SSE stream wrapper) and reqwest's
`stream` feature (gsp's `Response::chunk()`).

**Next**: slice 5 — revision history + diff + one-key rollback endpoints,
plus minimal bearer-token auth on the controller's API (`docs/08` phase
10+11); then the aggregator slices (6–10).

### Known follow-ups (none blocking)

| Item | Notes |
|------|-------|
| `sendmmsg` UDP egress batching | reply pump + upstream forward still one `send` per datagram; per-session reply buffers of `RECV_BATCH`×`MAX_DATAGRAM` would 16× RSS — needs a smaller batch buffer or per-datagram alloc, its own decision |
| Per-source cap + UDP sticky table: LRU eviction | both refuse / wholesale-clear when full today; acceptable defaults — do only if load testing shows them biting |
| k8s discovery watch informer | polling Endpoints now; a convergence-speed optimization, belongs with the fleet-phase discovery rework |
| Resolver `sticky_key` | deferred pending a design for how a later request recovers the key; overlaps the phase-11 intent model |
| `IPV6_TRANSPARENT` on musl / non-glibc | `set_ip_transparent` already calls `socket2` 0.6's `set_ip_transparent_v6` unconditionally — may already work; build + smoke-test on a musl target before writing code |
| Retire the UDP sticky table via `consistent_hash` | pure polish, no user-visible gap |
| Reload debounce only coalesces within one 200 ms window | wider-spaced events cause separate (idempotent) reloads; low priority |
| NFR N3/N4/N5/N9 (aggregate throughput, full 500k/1M, HA) | need dedicated hardware + multiple hosts + a real load generator; the `gsp-bench --mode concurrency` ramp already went as far as one box allows (~20k TCP / ~5k UDP verified here) |
| More sniffers (`quic`, `wireguard`, …), multiple sniffers per listener | community / plugin ecosystem; never blocks core work |

---

## Workflow gotcha: run `cargo fmt --all` as its own step before `make check`

CI once failed a `cargo fmt --all --check` even though `make check` had reportedly
passed locally, because `cargo fmt --all` was run early in the session and more
code was `Edit`ed in afterwards. `--check` (which `make check` runs) only
*reports* diffs and exits non-zero — it never rewrites the file. So:

**Immediately before every `make check` / commit, run `cargo fmt --all` (the
writing form, no `--check`) as its own step** — never assume an earlier fmt pass
covers later edits. Piping `make check` through `tail` also hides an early
`fmt-check` failure message; check the exit code or read from the top.

---

## Invariants that bite if you forget them

(Full list in [`CLAUDE.md`](CLAUDE.md) "Architecture invariants". These are the
ones with a subtlety.)

- **Exactly one place builds + stores a `Snapshot`: `reload::apply`.** Runtime
  edits (backend overlay, discovery refresh, admin) are just extra *rebuild
  triggers* — they mutate a persistence layer (`BackendOverlay`, `Discovery`)
  then call `request_reload()`. Backend *state* (`PATCH`) mutates atomics on the
  live `Backend` and needs no rebuild.
- **`Sniffers` and `Resolvers` registries are `ArcSwap`-backed** (unlike the
  snapshot, which every worker re-`load`s itself). Every worker clone points at
  the *same* instance, so `replace()` from the reload task is visible instantly
  with no replumbing.
- **Listeners are reconciled by name** (`ListenerManager::reconcile`), not
  rebuilt with the snapshot. An unchanged listener keeps running with its route
  rules captured at spawn; only the *pool contents* they resolve to are read
  live. A `ListenerConfig` change (bind, protocol, routes, affinity, `prefix`,
  `freebind`, `route_hint`, ACL, `rate_limit`, `geo`, `transparent`) stops and
  re-spawns it — a same-bind rebind is gapless via `SO_REUSEPORT`.
- **Startup-only config** (reload does *not* re-read): `workers`,
  `settings.limits`, `settings.geo_db`, `settings.sniffers` engine params
  (`call_timeout_ms` / `max_memory_bytes` — only the `dir` *contents* are live).
- **Zero `unsafe`.** `socket2` 0.6 (`SockRef` for `IP_TRANSPARENT` v4/v6,
  `IP_FREEBIND`), `nix` (`IP_PKTINFO` / `IP_ORIGDSTADDR` cmsgs, `recvmmsg`,
  `splice`) — all safe wrappers.
- **UDP amplification guard**: the proxy never sends to a client without an
  established session; a dropped datagram (routing / ACL / rate / gate) gets no
  reply. Covered by `crates/gsp-core/tests/amplification.rs`.

---

## Latency ledger

Per-connection / per-datagram cost of every feature. **If you add a
per-connection or per-datagram task, hop, or allocation, add it here.**

### Steady state (per byte / per datagram)

- **TCP pump**: on Linux, `splice(2)` `socket → pipe → socket` — no userspace
  copy, no 32 KiB×2 heap buffers (the pipe fds cost ~2 KiB kernel each). Up to 2
  extra syscalls per direction per 64 KiB chunk. Non-Linux / `pipe2` failure:
  buffered `try_read`/`try_write`, one 32 KiB buffer per direction. No lock.
- **UDP ingress**: a share of one `recvmmsg` (≤16 datagrams/syscall on Linux;
  fresh `MultiHeaders` amortised over the batch), one `HashMap` lookup by client
  `SocketAddr`, one relaxed atomic store (liveness), one `send` upstream. No
  lock, no alloc, no task spawn. Reply path is still one `recv` + one
  `send`/`send_to` per datagram (`sendmmsg` egress deferred).
- Recv buffers: `RECV_BATCH` (16) × 64 KB **per worker** (shared across that
  worker's sessions) + one 64 KB buffer per reply task.
- Established UDP sessions skip the ACL / rate-limit / geo / gate checks
  entirely — those run only for datagrams that miss the session table.

### Per new TCP connection / new UDP session (paid once)

- **Base**: 1 `Pool::acquire[_for]` (lock-free reads + one atomic add), 1
  upstream `connect`, 1 spawned pump/reply task, (UDP) 1 socket `bind`+`connect`
  + sticky-table + session-table insert.
- **Routing**: 1 `local_addr()` syscall + a linear scan of the small route list
  (bit-compare per `client_cidr`/`dst`, `u16` range per `port`, `starts_with` +
  len check per `first_bytes`, one `extract_sni` pass per `sni`).
- **Peek** (only if a route uses `first_bytes`/`sni`/`sniffer`): one `MSG_PEEK`
  into a `peek_len()`-sized `Vec` (`PEEK_MAX` = 4096 for `sni`/`sniffer`), 250 ms
  budget, + an `Arc<ListenerConfig>` clone into the per-conn task. Re-peeks every
  5 ms only if the first byte is a TLS record and the first record isn't whole.
- **`consistent_hash`**: `O(healthy)` — one `sort_by_key` with a `DefaultHasher`
  per backend over the healthy `Vec` every balancer already builds. Same work as
  `least_conn`. Process-stable hash only (fine: reload rebuilds pools, no
  cross-instance state).
- **`route_hint: true`**: one `ArcSwap::load` + `HashMap::get` (lock-free) + one
  `String` clone when a hint is present. Listeners without the flag pay nothing.
- **External resolver**: one `.await`ed HTTP/gRPC round-trip (`timeout_ms`,
  default 40 ms) on the routing path, in the spawned task — never the accept
  loop. With `cache:` a repeat key is a `Mutex<LruCache>` get instead. A
  `target` connection skips `Pool::acquire_for` entirely — cheaper than pooled.
- **Sniffer plugin** (`WasmSniffer`): one `Store::new` + `Instance::new` (fresh
  per call) + `memory.write` of the peeked bytes + one guest call + decode, all
  synchronous on the per-conn task. Bounded by `call_timeout_ms` (epoch
  interruption) + `max_memory_bytes`. **Benchmarked**: p50 8–10 µs, p99 12–26 µs
  for the three first-party plugins — comfortably inside NFR N1 (500 µs), so the
  fresh-`Store`-per-call design needs none of the `InstancePre` / pooling /
  warm-instance fallbacks held in reserve. Listeners without a `sniffer:` route
  pay one `HashMap::get`. See `docs/07` "Sniffer plugin sandbox guarantees".
- **Filter chain** — CIDR ACL: a bounded bit-walk of the `deny` (and, if
  non-empty, `allow`) radix trie, ≤32/128 hops, no alloc/lock. GeoIP: one
  MaxMind tree lookup + a small `Vec` scan. Rate limit / `per_source` / global
  caps: one `Mutex<HashMap>` or atomic op, one RAII guard, no `.await`, no alloc.
  Each unconfigured filter is one `is_empty()` / `is_enabled()` bool check.
- **UDP first-packet gate**: reuses the already-computed sniff hint + a short
  route-list scan for a `FirstBytes` match. No lock/alloc/task.
- **PROXY protocol** (`v1`/`v2`): one `Vec` (≤52 B) + one extra `write_all` to
  the backend before the pump. `v2-udp`: one `Cow::Owned` on the *first*
  datagram only. `none` pays nothing.
- **Transparent mode**: upstream socket gains one `setsockopt(IP_TRANSPARENT)` +
  one `bind` before connect; UDP also binds one per-session `IP_TRANSPARENT`
  reply socket and uses `recvmsg` + a fixed cmsg buffer. A few extra syscalls at
  setup; nothing per byte.
- **UDP passive health**: one `Arc<Backend>` clone per session; on a `send`/`recv`
  error only, one `kind()` compare and (for `ConnectionRefused`) one
  `Backend::observe(false)`.
- **Connection draining** (`ConnTracker`): one `watch::send_modify` on open and
  on close. Nothing steady-state.

### Control-plane only (zero data-path cost)

Listener reconcile, backend overlay rebuild, backend discovery refresh
(`refresh_loop` per source → `fetch` → `Discovery::store` + `notify` → snapshot
rebuild reads `Discovery::get`).

---

## Codebase map

| File | Responsibility |
|------|----------------|
| `crates/gsp-config/src/lib.rs` | Raw YAML types, `validate()`, resolved `Config` / `PoolConfig` / `ListenerConfig` / `ResolverConfig` / `SniffersConfig` / `HealthCheck`; routing (`Matcher`, `Action`, `OnError`, `Cidr`, `CidrSet` trie, `HostPattern`, `MatchContext`, `RouteHint`, `extract_sni`); filters (`Acl`, `GeoAcl`, `RateLimit`, `PerSourceLimit`, `GlobalLimits`). **All schema rules here.** |
| `crates/gsp-core/src/snapshot.rs` | `Snapshot { listeners, pools, sources, resolvers, limits, geo_db }`; `build` → `build_with_overlay` → `build_with_sources` carry health/admin-state over by address and apply overlay + discovered backends. |
| `crates/gsp-core/src/pool.rs` | `Pool` (balancer, `rr` index, `hash_on`, `weights`; `acquire` / `acquire_for` / `acquire_addr` / `backend`, `hrw_score`), `Backend` (health / active / streaks / `check_kind` + `AdminState`), `BackendGuard` (RAII slot + passive health), `PickError`. |
| `crates/gsp-core/src/listener.rs` | `run_tcp_listener`: accept loop; per-conn task does ACL/geo/rate/`per_source`/global-cap checks, first-bytes peek, route match, pool lookup. |
| `crates/gsp-core/src/listener_udp.rs` | `run_udp_listener`: per-worker `recvmmsg` batch loop, `(client, Option<SocketAddr> dst)` session table, sticky affinity, `IdleWheel` idle expiry, per-session upstream socket + reply pump. `UdpMode` Plain / Prefix (`IP_PKTINFO` + `sendmsg` reply) / Transparent (`IP_ORIGDSTADDR`, client-bound upstream, per-session `IP_TRANSPARENT` reply socket). |
| `crates/gsp-core/src/{ratelimit,src_conns,limits,geo}.rs` | Per-listener token bucket / per-source concurrent cap / process-wide caps / MaxMind country lookup. |
| `crates/gsp-core/src/sniff.rs` | `Sniffer` trait + `Sniffers` `ArcSwap`-backed registry (`register` / `get` / `replace`) + `warn_if_missing`. **No built-in sniffers** — the seam the phase-9 loader fills. |
| `crates/gsp-core/src/route_hint.rs` | `RouteHints` — `ArcSwap<HashMap>` `src_ip → pool` push-resolver table (`POST /route-hint`), lock-free read. |
| `crates/gsp-core/src/resolver.rs` | `trait Resolver`, `ResolveRequest` / `Resolution` / `ResolveError`, `Resolvers` (`ArcSwap`-backed) map, `resolve_route` (async route walk), `CachedResolver` (TTL LRU). Transports live in `gsp`. |
| `crates/gsp-core/src/discovery.rs` + `sources.rs` | `trait BackendSource` + `Discovery` last-known-good cache + `refresh_loop`; `SourceManager` reconciles one refresh task per pool `source` on reload (discovery analogue of `ListenerManager`). |
| `crates/gsp-core/src/drain.rs` | `ConnTracker` / `ConnGuard` — `watch<usize>` live count + an `id → {proto, listener, peer, local, pool, backend, since}` registry (`GET /sessions`); `wait_idle()`. |
| `crates/gsp-core/src/overlay.rs` | `BackendOverlay` — runtime backend add/remove, layered on file `targets` at rebuild. |
| `crates/gsp-core/src/proxy.rs` | `handle_tcp` (pool) / `handle_tcp_target` (resolver `target`, no guard) → `connect_backend` + `pump` (`splice` / buffered). Writes the PROXY protocol header before the pump. |
| `crates/gsp-core/src/proxy_protocol.rs` | `header(mode, src, dst)` — PROXY protocol v1 (text) / v2 (binary, STREAM or DGRAM). Write-only. |
| `crates/gsp-core/src/health.rs` | 500 ms sweep, probes due backends (`tcp_connect` / `udp_probe`), updates health + `gsp_pool_backends` gauges. |
| `crates/gsp-core/src/runtime.rs` | `Runtime::start*` builds `ListenerManager` + `SourceManager` + health task; owns `RouteHints` / `ConnTracker` / `BackendOverlay` / `Sniffers` / `reload_requested`. `shutdown_with_grace` = stop listeners + await health + `wait_idle` + `abort_all`. |
| `crates/gsp-core/src/listeners.rs` | `ListenerManager` — one task `Group` (workers + `watch<bool>` stop) per listener; `start_all`, `reconcile(&Snapshot)` (diff by name), `stop_all` / `abort_all`. |
| `crates/gsp-core/src/net.rs` | `bind_reuseport_tcp` (+ `freebind` / `transparent`), `bind_reuseport_udp` (`UdpMode`), `bind_transparent_udp`, `connect_tcp_from`, `set_ip_transparent` (v4 + v6). |
| `crates/gsp-core/src/metrics_defs.rs` | **Every metric name** (`pub const`). |
| `crates/gsp/src/main.rs` | CLI (`--config`, `--check`), tracing, runtime bring-up, shutdown. Opens `geo_db` + builds sniffers (fails `--check` / startup on a bad one). |
| `crates/gsp/src/admin.rs` | axum router: `GET /healthz` `/readyz` `/metrics` `/pools` `/config` `/sessions`; `POST /route-hint` `/admin/drain` `/admin/undrain`; `PATCH` backend state; `POST` / `DELETE` a backend. `router(state)` split out for the in-module HTTP tests. |
| `crates/gsp/src/resolver.rs` | `HttpResolver` (`reqwest`), `GrpcResolver` (`tonic`, `mod pb` from `build.rs`), `build_resolvers`. |
| `crates/gsp/src/sniffer_loader.rs` | `SnifferLoader` (shared `wasmtime::Engine` + epoch-ticker thread) + `scan(&SniffersConfig)`; `WasmSniffer`; `build_sniffers` = `new` + one `scan`. |
| `crates/gsp/src/discovery.rs` | `DnsSrvSource` (`hickory-resolver`), `ConsulSource` / `KubernetesSource` (`reqwest`), `DiscoveryFactory`. |
| `crates/gsp/src/reload.rs` | `SIGHUP` + `notify` file watch + `reload_requested()` → debounce → `apply` (validate, `build_with_overlay`, store, reconcile listeners / sources / resolvers, rescan sniffers). |
| `crates/gsp/src/procinfo.rs` | `gsp_build_info` / `gsp_fd_open` / `gsp_fd_limit` — build identity + a small detached `/proc/self/fd` sampling task. |
| `crates/gsp/proto/resolver.proto` + `build.rs` | gRPC resolver contract + `tonic_build` codegen (needs `protoc`). |
| `crates/plugins/` | Standalone workspace (own `[workspace]`): `gsp-sniffer-abi` guest helper + `a2s` / `minecraft` / `regex-firstbytes` plugins. `make plugins`. Never a dep of `gsp` / `gsp-core`. |
| `crates/gsp-bench/` | `make bench` — `latency` mode (in-process, added p50/p99 vs. NFR N1/N2) + `concurrency` mode (real separate `gsp` process, connection-count ramp, `/proc` RSS/fd sampling). |
| `crates/gsp-config/fuzz/` | Standalone workspace: `extract_sni` / `route_match` / `parse_config` `cargo-fuzz` targets. `make fuzz` (nightly). |

---

## Testing

`make check` runs fmt + clippy `-D warnings` + ~150 tests (`gsp-config`,
`gsp-core` unit + `crates/gsp-core/tests/{tcp_forward,udp_forward,amplification}.rs`,
`gsp` unit incl. the `sniffer_loader` WAT-fixture end-to-end and the in-module
admin HTTP tests). Needs `protoc` on `PATH`.

- `TP­ROXY` / `IP_TRANSPARENT` e2e is not in CI (needs `CAP_NET_ADMIN`) — covered
  by config parse/reject + `connect_tcp_from` fallback tests. Setup recipe in
  `docs/04`.
- The UDP `prefix:` e2e needs a Linux host with `IP_PKTINFO` and loopback
  `127.0.0.2` / `127.0.0.3`; not portable to macOS/Windows CI.
- `#[ignore]`d, run in the `plugins` CI job after `make plugins`: the first-party
  `.wasm` artifact round-trip and the WASM-boundary N1 latency bench.
- `wasmtime` is a normal `cargo` dep — it does **not** need the
  `wasm32-unknown-unknown` rustc target; that target is only needed to *build*
  the plugin crates. Loader tests assemble WASM from inline WAT via the `wat`
  crate.

---

## Infra / environment

- Toolchain via `rustup` (`stable`). If `cargo` isn't found:
  `export PATH="$HOME/.cargo/bin:$PATH"`.
- **`protoc` is a build requirement** (gRPC resolver codegen in
  `crates/gsp/build.rs`). CI installs `protobuf-compiler`.
- CI: `.github/workflows/ci.yml` — installs `protoc`, then `cargo fmt --check`,
  `clippy --all-targets --all-features`, `cargo test --all`. Plus nightly `fuzz`
  and `plugins` jobs.
- git remote `github.com/Wueschli/gameserver-proxy`, branch `main`; `git push`
  works, `origin/main` is current. The HTTPS credential helper logs a harmless
  "nonexistent Windows path" warning before falling back to a working credential.
- `Cargo.toml` declares `MIT OR Apache-2.0` with `LICENSE-MIT` / `LICENSE-APACHE`
  included. No explicit user decision on record — confirm if it matters.

---

## Open questions carried from `docs/01-requirements.md`

- Does one client ever need **two backends at once** (TCP control + UDP gameplay
  on different instances)? Affects the session model.
- Is **QUIC-aware routing** (connection ID) needed, or is opaque UDP enough?
  Assumed opaque.
- Cross-instance session failover: assumed **no** for v1 (ADR 4).
