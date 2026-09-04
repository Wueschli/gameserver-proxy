# 08 – Roadmap

Incremental. Each phase is usable on its own.

Status legend: ✅ done · 🔜 next · ⬜ planned.

## Phase 0 – Skeleton ✅
- ✅ Project setup (Cargo workspace: `gsp-config`, `gsp-core`, `gsp`), CI (fmt +
  clippy `-D warnings` + tests), test harness.
- ✅ Config loading + validation + immutable snapshot behind `ArcSwap`.
- ✅ Structured logging (`tracing`), `/healthz`, `/readyz`, `/metrics`, `build_info`.

## Phase 1 – L4 TCP proxy ✅
- ✅ TCP listener with `SO_REUSEPORT`, one accept task per core.
- ✅ Static listener→pool mapping, `round_robin` + `least_conn`.
- ✅ Bidirectional pump; per-direction idle timeout, connect timeout, half-close.
  Linux `splice(2)` zero-copy fast path landed in the perf pass (ADR 17) —
  `socket → pipe → socket` in the kernel, buffered `try_read`/`try_write`
  fallback elsewhere, both behind the same `proxy::pump` fn.
- ✅ Active `tcp_connect` health checks (`rise`/`fall`), passive connect-failure
  feedback, backend healthy/unhealthy state, per-backend session caps.
- ✅ Hot reload on `SIGHUP` / config-file change: rebuild snapshot, atomic swap,
  backend health carried over by address. Listener bind/protocol/pool-mapping
  changes still need a restart (full listener reconfiguration is phase 5).
- ✅ Core metrics: connections, bytes, duration, backend connect errors, pool
  backend counts, healthcheck results, LB selections, config reload/version.
- **Result**: usable as a plain TCP game server front proxy.

## Phase 2 – L4 UDP proxy ✅
- ✅ UDP listener with `SO_REUSEPORT`, one recv task per core, worker-local
  (lock-free) session table. Ingress batches — one `recvmmsg(2)` pulls up to
  `RECV_BATCH` datagrams per wakeup (perf pass, ADR 18). Idle expiry is a
  single-level timing wheel (perf pass, ADR 19). `sendmmsg` egress batching is
  still deferred.
- ✅ Per-session `connect(2)` upstream socket + a reply-pump task per session.
- ✅ Session affinity (`hash_on: src_ip | src_ip_port` + per-worker sticky
  table). (`consistent_hash` balancer deferred.)
- ✅ `udp_probe` health check (`send_hex` / `expect_hex_prefix`).
- ✅ Per-backend session caps (shared with TCP) + per-pool idle timeout;
  amplification guard (no reply without an established session).
- **Result**: covers the majority of real-time game servers.

## Phase 3 – Routing intelligence (week 8–10) ✅
- ✅ Route rule list with priorities (`listeners[].routes`, first match wins;
  bare `pool:` normalised to one `always` route).
- ✅ Matchers: `always`, `port` (destination port), `client-cidr` (source IP).
- ✅ `first-bytes` matcher — `prefix` (`hex:` / `ascii:`, ≤ 512 B) and/or
  `length: { min, max }`; TCP peek / first UDP datagram. (Regex-over-first-bytes
  moved to Phase 9 — it is a plugin concern, not a `first-bytes` sub-form.)
- ✅ `consistent_hash` balancer (rendezvous/HRW hash, pool `hash_on: src_ip |
  src_ip_port`) — session affinity without a sticky table.
- ✅ SNI peek matcher (`sni`, `host` exact / `*.suffix` / `.suffix`; ClientHello
  peeked, not terminated; TCP only).
- ✅ `dst` matcher (destination IP vs `cidrs`). ✅ UDP prefix listener
  (`prefix: <cidr>`, one wildcard `IP_PKTINFO` socket serving the whole routed
  prefix, replies from the hit address) + TCP `freebind`. ✅ push resolver
  (`POST /route-hint`, per-listener `route_hint: true`).
- ✅ Sniffer API **seam** — `gsp_core::sniff::Sniffer` → `RouteHint`, the
  `sniffer` matcher, and the listener wiring that feeds a hint into routing.
  **No built-in sniffers ship** (game-specific parsing does not belong in the
  proxy binary); a `sniffer:` route only matches once a plugin is loaded. The
  loader is Phase 9.
- **Result**: multiple games/regions behind one port (via `dst` / `sni` /
  `first-bytes`; game-protocol sniffing once plugins land).
- ✅ F1.4's port-range listener (`docs/01-requirements.md`) — `bind:
  "host:lo-hi"` (e.g. `"0.0.0.0:30000-30999"`) spawns one real socket per
  port (× `workers`, `SO_REUSEPORT`-shared) under one listener config, for
  dedicated-server fleets that dynamically pick a port per match/instance
  (Agones-style `hostPort` ranges, Source/UE servers auto-incrementing past a
  taken default port) and for scheme B (subdomain-per-port) at real scale.
  This requirement was written down at project start and not carried into an
  implementation slice for a long time — caught by a documentation audit, not
  by design — then closed out right after. `gsp_config::ListenerConfig::bind`
  (primary/lowest port) + `extra_binds` (the rest); capped at 1024 ports per
  range. Mutually exclusive with `prefix` (which needs exactly one wildcard
  socket). The `port` *route* matcher (which already supported a `"lo-hi"`
  range within one listener's routes) is what actually splits pools by port
  within the range — the bind range just makes the sockets exist.

## Phase 4 – External routing logic (week 11–12) ✅ (sticky_key deferred)
- ✅ **Slice 1**: `resolvers:` config + `action: { resolver: <name> }`; the
  `Resolver` trait + async routing loop in `gsp-core`; `HttpResolver` (reqwest)
  in `gsp`; `pool` results; `on_error: reject | fallback_route`.
- ✅ **Slice 2**: result cache — configurable key (`src_ip` / `src_ip_port` /
  `sni` / `routing_key` / `first_bytes:a:b`), positive/negative TTL,
  `max_entries` LRU (`lru` crate), `on_error: stale_ok`.
- ✅ **Slice 3**: gRPC transport (`tonic` + `prost`, `proto/resolver.proto` +
  `build.rs`; CI installs `protoc`).
- ✅ **Slice 4**: `Resolution.target` (fixed instance, pool-less connect path in
  `proxy.rs` / `listener_udp.rs` — no health / cap / guard). ⬜ `sticky_key`
  deferred (overlaps the request-keyed cache + `route_hint` + UDP affinity;
  needs its own design).
- ✅ **Post-phase (data-plane completion)**: `resolvers:` reloads live —
  `gsp_core::Resolvers` gained `ArcSwap` interior mutability (like `Sniffers`),
  and the reload task rebuilds + swaps the clients when (only when)
  `ResolverConfig` differs. Trade-off: a rebuild resets each `CachedResolver`'s
  LRU cache.
- ✅ **Post-phase (data-plane completion)**: `backend_sources:` reloads live —
  `gsp_core::SourceManager` (the discovery analogue of `ListenerManager`) owns
  one refresh task per pool `source` behind a private stop channel and
  reconciles them on every reload by diffing `Snapshot::sources`
  (pool → `SourceConfig`, newly echoed). Added / removed / re-parameterised
  entries start / stop / restart their task; a removed pool also drops its
  cached discovered set. `gsp-core` stays HTTP-free — the `gsp` binary supplies
  a `SourceFactory` (`DiscoveryFactory`) that rebuilds the concrete adapter.
- **Result**: matchmaker integration, token→instance routing.

## Phase 5 – Operations & zero-downtime (week 13–14) ✅
- ✅ Hot reload (SIGHUP + file watch), atomic snapshot swap. *(phase 1)*
- ✅ **Slice 1**: `enabled` / `draining` / `disabled` backend states —
  `AdminState` on `Backend`, excluded from new-session selection (incl. UDP
  affinity) while existing sessions drain; carried across reload by address;
  `PATCH /pools/{p}/backends/{addr}`; `gsp_pool_backends{state=draining|disabled}`.
- ✅ **Slice 3**: `POST /admin/drain` / `POST /admin/undrain` (flip `readyz`
  without stopping the data path) + `GET /config` (plaintext snapshot view with
  `draining` / `active_conns`).
- ✅ **Slice 4**: backends CRUD — `POST /pools/{p}/backends {addr}` /
  `DELETE /pools/{p}/backends/{addr}`; edits held in a runtime `BackendOverlay`
  layered on the file config (survives a file reload). An edit calls
  `request_reload()` and the existing reload path rebuilds via
  `Snapshot::build_with_overlay`.
- ✅ **Slice 2**: graceful draining of the **proxy instance** — a `ConnTracker`
  counts in-flight TCP connections + UDP sessions; `SIGINT`/`SIGTERM` stops the
  accept/recv loops and `Runtime::shutdown_with_grace` waits for the tracked work
  to finish, bounded by `settings.shutdown_grace_sec` (default 30). UDP listeners
  keep pumping established sessions until they idle out; new datagrams while
  draining are dropped (`reason="draining"`).
- ✅ **Slice 5**: runtime listener add / remove / rebind — `ListenerManager`
  owns one task group per listener; a reload reconciles them by name (`SO_REUSEPORT`
  makes a same-bind rebind gapless).
- ✅ `GET /sessions` — live connection / session registry on the connection
  tracker (`id → {proto, listener, peer, local, pool, backend, age}`), exposed
  as a filterable plaintext admin endpoint. *(data-plane completion, list C)*
- ✅ Passive health signals from the data path. *(phase 1)*
- **Result**: a production-ready deploy/update cycle.

## Phase 6 – Client-IP preservation (week 15–16)
- ✅ **Slice 1**: PROXY protocol v1/v2 (TCP) — per-pool `proxy_protocol:
  none | v1 | v2`; one header prepended to the upstream connection before any
  client bytes (`gsp_core::proxy_protocol`), `gsp_proxy_protocol_headers_total`.
- ✅ **Slice 2**: v2-UDP variant — `proxy_protocol: v2-udp` (UDP-only, validated)
  prepends the v2 binary header to the **first datagram** of each session; later
  datagrams are untouched.
- ✅ **Slice 3**: transparent mode (TPROXY) for **TCP** — `transparent: true`
  (Linux) binds the listen socket with `IP_TRANSPARENT` and every upstream
  connection with the real client `ip:port` as its `IP_TRANSPARENT` source;
  `docs/04` carries the nftables + policy-routing setup.
- ✅ **Slice 4**: UDP transparent mode — `IP_TRANSPARENT` + `IP_RECVORIGDSTADDR`
  on the listen socket (original `ip:port` per datagram), a client-bound
  `IP_TRANSPARENT` upstream socket, and a per-session `IP_TRANSPARENT` reply
  socket bound to the original destination. `IPV6_TRANSPARENT` listen binds via
  the socket2 0.5 → 0.6 bump. Mutually exclusive with `prefix`.
- **Result**: backends see the real client IP (PROXY protocol or fully
  transparent, TCP + UDP).

## Phase 7 – Security & hardening (week 17–18)
- ✅ **Slice 1**: CIDR allow/deny filter chain — per-listener `allow` / `deny`
  CIDR lists, checked on the client source IP before routing (TCP accept + UDP
  first datagram). `deny` wins; a non-empty `allow` is default-deny. Blocked =
  silent drop + `gsp_filter_blocked_total{listener,filter="acl"}`. Linear scan of
  the (small) `Cidr` list; established UDP sessions are not re-checked per
  datagram. (Slice 6 replaced the scan with a radix trie.)
- ✅ **Slice 2**: per-listener token-bucket rate limit on new connections / new
  UDP sessions — `rate_limit: { per_ip, per_net }` (`rate` permits/s + `burst`),
  `per_net` keyed by /24 (v4) / /64 (v6), checked after the ACL. A permit needs
  every configured bucket; excess dropped silently +
  `gsp_filter_blocked_total{filter="rate_ip"|"rate_net"}`. One `Mutex<HashMap>`
  per listener shared across workers, lazy prune of idle buckets. Established UDP
  sessions keep a scan-free steady path.
- ✅ **Slice 3**: global caps — `settings.limits.{max_connections,
  max_udp_sessions, max_new_sessions_per_sec}` (all optional; startup-only). Live
  `AtomicUsize` counters for TCP conns / UDP sessions + a token bucket for the
  new-session rate, held in one `gsp_core::limits::GlobalLimits` shared by every
  worker. A new connection / session over a cap is refused before allocation
  (existing untouched) + `gsp_filter_blocked_total{filter="max_conn"|"max_udp"|
  "max_new_rate"}`. RAII `LimitGuard` releases the slot on connection / session
  end.
- ✅ **Slice 4**: UDP first-packet gate — `first_packet_gate: true` on a UDP
  listener opens a session only when the first datagram is positively recognised
  (a non-`reject` sniffer hint, or a matching `first_bytes` route). Checked
  before the `route_hint` lookup; unrecognised ⇒ no session, no reply,
  `gsp_datagrams_dropped_total{reason="first_packet_gate"}`. `validate()`
  rejects it on TCP or with nothing to gate on.
- ✅ **Slice 5**: automated amplifier-checklist tests
  (`crates/gsp-core/tests/amplification.rs`) — no unsolicited / duplicated
  replies, no error reply to a dropped datagram, reply size == backend payload
  (proxy adds nothing toward the client), rate limit enforced before any state
  change. Checklist in `docs/07` now ticked.
- ✅ **Slice 6**: ACL longest-prefix-match trie — `gsp_config::CidrSet`, a binary
  radix trie over address bits (v4 / v6 separate), built once per listener spawn
  from the `allow` / `deny` lists. `Acl::permits` now does a bounded bit-walk
  instead of a linear `Cidr` scan, so large threat-feed block lists cost the
  same as a handful of entries. Config, semantics and `GET /config` output
  unchanged.
- ✅ **Slice 7**: optional GeoIP country filter — `settings.geo_db` (a MaxMind
  Country `.mmdb`, opened once at startup / `--check`) + per-listener
  `geo: { allow, deny }` (ISO 3166-1 alpha-2), checked right after the CIDR ACL
  on the client source IP, same precedence. `gsp_config::GeoAcl` holds the
  decision; `gsp_core::geo::GeoDb` (dep: `maxminddb`) does the lookup. Fails
  closed if the DB isn't loaded. `gsp_filter_blocked_total{filter="geo"}`.
- ✅ **Slice 8**: `cargo-fuzz` harnesses for the untrusted-input parsers
  (`crates/gsp-config/fuzz/`): `extract_sni` (TLS ClientHello reader),
  `route_match` (matchers + `extract_sni` + host patterns over a fuzzed
  first-bytes buffer), `parse_config` (`parse_str` on arbitrary input). Seeds in
  `fuzz/seeds/`, `make fuzz`, and a nightly CI job. No crashes in the initial
  runs (10M+ execs on `extract_sni`).
- ✅ **Slice 9**: per-source concurrent connection / session cap —
  `per_source: { max_per_ip, max_per_net }` on a listener bounds how many
  connections / UDP sessions are live at once from one client IP / /24 (v4) /
  /64 (v6), where `rate_limit` bounds the *rate*. `gsp_core::src_conns::SourceLimiter`
  (live counters + RAII `SourceGuard`), checked after `rate_limit`, released on
  connection close / session eviction. `gsp_filter_blocked_total{filter="src_conn_ip"|"src_conn_net"}`.
- ✅ **Slice 10**: `crates/gsp-bench` — a single-host latency/load harness
  (`make bench`) that measures the proxy's *added* request→response latency
  (p50 / p99) against **NFR N1** (`< 0.5 ms`) / **N2** (`< 2 ms`), with an
  optional busy-connection load knob, informational single-stream throughput and
  a loose idle-RSS-per-connection number. N3 (aggregate throughput), N4/N5
  (500k / 1M) and N9 (HA) still need dedicated hardware + a real load generator.
- **Result**: hardened against common L4/7 abuse; per-connection overhead
  measurable via `make bench`.

## Phase 8 – Discovery & scaling (week 19–20) ✅
- ✅ `BackendSource` seam in `gsp-core` (`discovery.rs`: trait + `Discovery`
  last-known-good cache + `refresh_loop`), concrete adapters in the `gsp` binary
  (`discovery.rs`: `DnsSrvSource` via `hickory-resolver`, `ConsulSource` /
  `KubernetesSource` via `reqwest`). Same HTTP-free seam as resolvers.
- ✅ **Level-triggered**: a source returns the current address set; the runtime
  diffs it against the live set. One control-plane refresh task per source
  (`refresh_interval_sec`), never on the data path (zero data-path cost —
  latency ledger).
- ✅ Fed through the existing snapshot rebuild
  (`Snapshot::build_with_sources` ← `reload::apply`): one snapshot writer.
  Precedence: `discovered set (or file seed) ∪ overlay-added − overlay-removed`,
  then health / admin state.
- ✅ Degrade to last-known-good: an errored / empty refresh keeps the previous
  set (never clears the pool) + `gsp_discovery_refresh_total{result}` /
  `gsp_discovery_backends`.
- ✅ Config: top-level `backend_sources:` list referenced by `pools[].source`
  (exactly one of `targets` / `source`). `static` is folded into the pool's
  targets at load time.
- ✅ k8s uses **polling** (`GET .../endpoints/<svc>` with the in-pod SA token +
  CA). Deferred: a watch-based informer (fewer API calls, faster convergence).
- ✅ HA operations chapter in `docs/06` — anycast vs. L4 LB, capacity planning
  per instance, dashboards & alerts.
- **Result**: dynamic backend fleets, horizontal scaling.
- Deferred: k8s watch informer.
- ✅ `backend_sources:` live reload (`SourceManager`) — landed in the
  data-plane-completion pass; see Phase 4's post-phase note.

## Phase 9 – Sniffer plugin loader

Game-protocol sniffers load into a running proxy from disk, sandboxed, never
compiled in and never a fork. A `sniffer:` route resolves its name against the
loaded set (today that always misses — `gsp_core::sniff::sniffer` returns `None`
for every real name).

### Locked decisions
- **Sandbox: `wasmtime`, core module, no WASI.** A narrow ABI — the guest
  exports `memory`, `alloc(u32) -> u32`, and
  `sniff(ptr: u32, len: u32) -> u64` (packed `ptr<<32 | len`; `0` = not
  recognised); the result bytes are a compact encoding of `RouteHint`. No WASI,
  no host functions ⇒ a plugin cannot touch the filesystem, clock, or network.
- **Time bound: epoch interruption.** A host thread calls
  `Engine::increment_epoch()` every `call_timeout_ms`; a plugin that overruns
  traps. (Fuel is optional on top.)
- **Memory bound:** `StoreLimits` `max_memory` + one instance per call, from
  `settings.sniffers.max_memory_bytes`.
- **Host lives in the `gsp` binary.** `wasmtime` is a binary-only dep, like
  `reqwest` / `hickory-resolver`; the `Sniffer` trait and the
  `MatchContext.sniff` wiring stay in `gsp-core` (crate-boundary rule 7).
- **Registry threading.** The free fn `sniff::sniffer(name)` (global, `'static`)
  becomes an `Arc<Sniffers>` map (`HashMap<String, Arc<dyn Sniffer>>`) threaded
  runtime → `ListenerManager` → workers, exactly like `Arc<Resolvers>` /
  `Option<Arc<GeoDb>>`. This is the one real refactor.
- **First-party plugins ship built, the same way:** `a2s`, `minecraft`, and a
  generic bounded `regex-firstbytes` (keeps `regex` off the core routing path).
  `sni` stays a native `Matcher` — it is not a sniffer.
- **Latency is a gate, not an assumption.** Bench the WASM boundary vs. NFR N1
  (< 0.5 ms added) before wiring it on the per-connection path. Fallbacks, in
  order: `InstancePre` + a pooling allocator; one warm instance per worker reset
  between calls; ultimately the out-of-process Phase-4 resolver.

### Config (`settings.sniffers`)
`dir` (\*.wasm, loaded at startup, rescanned on reload), `call_timeout_ms`,
`max_memory_bytes`, optional `modules: [{ name, sha256 }]` for supply-chain
pinning. Per-listener `sniffer:` is unchanged; the input is already capped at the
route's `peek_len()` ≤ `PEEK_MAX`.

### Slices (all done — see `HANDOVER.md` codebase map + git history for detail)
- ✅ **Slice 1**: registry threading (pure refactor) — `sniff::sniffer` global fn
  → `gsp_core::sniff::Sniffers`, an `Arc<Sniffers>` threaded to every listener
  worker like `Arc<Resolvers>` / `Option<Arc<GeoDb>>`. No built-in sniffers
  register in production; no `wasmtime` yet.
- ✅ **Slice 2**: `settings.sniffers` schema (`dir`, `call_timeout_ms`,
  `max_memory_bytes`, `modules: [{ name, sha256 }]`) → `gsp_config::SniffersConfig`.
  Config-only — nothing reads it yet.
- ✅ **Slice 3**: `WasmSniffer` (`crates/gsp/src/sniffer_loader.rs`) — a shared
  `wasmtime::Engine` (epoch interruption) + epoch-ticker thread, a fresh
  `Store`/`Instance` per call under a `StoreLimits` memory cap and a one-tick
  epoch deadline. ABI: guest exports `memory`, `alloc(len) -> ptr`,
  `sniff(ptr,len) -> packed|0` (later widened by ADR 16a); host decodes a
  compact `RouteHint` — any bad pointer/length/UTF-8 is `bad_output`, never a
  panic. `build_sniffers(&SniffersConfig)` scans `dir`, verifies `sha256`
  pins, fails closed on a bad plugin (like `geo_db`).
  `gsp_sniffer_calls_total{name,result}` / `gsp_sniffer_call_seconds{name}`.
- ✅ **Slice 4**: live `dir` rescanning on reload — `Sniffers` gained
  `ArcSwap` interior mutability (mirrors `RouteHints`) so `replace()` from the
  reload task is visible to every worker instantly; a scan error keeps the
  previous plugin set. Engine params stay startup-only.
- ✅ **Slice 5**: first-party plugin crates in the standalone
  `crates/plugins/` workspace (own `[workspace]`, never a dep of
  `gsp`/`gsp-core`) — `gsp-sniffer-abi` (guest-side ABI helper) + `a2s`
  (Source-engine query packets), `minecraft` (Handshake `server address`,
  the BungeeCord/Velocity virtual-host trick), `regex-firstbytes` (a bounded
  pattern-matcher template, later made genuinely configurable by ADR 16a).
  `make plugins`; CI job `plugins`.
- ✅ **Slice 6**: WASM-boundary latency bench vs. N1
  (`sniffer_loader::tests::wasm_boundary_latency_vs_nfr_n1`, `#[ignore]`d,
  run in the `plugins` CI job). **Result: p50 8–10 µs, p99 12–26 µs** across
  all three plugins — comfortably inside N1, so the fresh-`Store`-per-call
  design needs none of the `InstancePre` / warm-instance fallbacks held in
  reserve. Found along the way: the epoch ticker runs on wall-clock time for
  the loader's whole lifetime, so `call_timeout_ms` is a *ceiling*, not a
  per-call guarantee — documented in `docs/07` "Sniffer plugin sandbox
  guarantees".
- ✅ **Slice 7 — phase 9 complete**: end-to-end test driving a live `Runtime`
  + real TCP connection through the real `build_sniffers` directory scan (a
  WAT fixture compiled inline, no wasm toolchain needed) — not `#[ignore]`d,
  runs in every `make check`.

- **Result — phase 9 complete**: runtime-loaded, sandboxed game-protocol
  sniffers (`a2s`, `minecraft`) plus a `regex-firstbytes` template, all
  measured inside NFR N1; `regex` first-bytes matching lives in an optional
  plugin, never in core. Known follow-ups, not blocking:
  per-source cap LRU eviction, a k8s discovery watch informer — see
  `HANDOVER.md`. (`GET /sessions` since landed — data-plane completion.)
- ✅ **Post-phase-9 (data-plane completion)**: per-plugin config. ADR 16a
  widens the guest ABI to
  `sniff(in_ptr, in_len, cfg_ptr, cfg_len)`; `settings.sniffers.modules[]`
  grows a `config` string, marshalled into a second linear-memory region on
  every call. `a2s` / `minecraft` ignore it; `regex-firstbytes` was rebuilt
  around it — a tiny `[key:NAME|] [@OFFSET ](hex:…|ascii:…){|…}` pattern
  language, so it is a genuinely runtime-configured bounded matcher and not the
  hard-coded HTTP template.

## Phase 10+11 (merged) – PoC: single controller + single aggregator
Full design: [10-distributed-control-plane.md](10-distributed-control-plane.md)
("Fleet topology", "The controller", "The aggregator", "The admin GUI"). This
is the **first working release** of the two new services, deliberately
scoped down from the full design: **one `standalone` controller tier, one
aggregator tier, `replicas: 1` on each, no `slave` role, no HA, no adoption
flow.** Those are real, already-designed extensions (see `docs/10`) but are
explicitly a *later* release, not part of this one — ship something that
works end-to-end first. **No change to the hot path or the `Snapshot`
invariant** in either slice group.

**Scope call locked for this release**: operator intent (backend
overlay/admin-state, route hints) stays exactly as it is today — per-instance,
via the existing admin API — and does **not** move into the controller yet.
The controller in this release owns **structural config only**. The
aggregator's intent fan-out (below) is a thin proxy to each instance's
existing admin verbs, not a new durable intent store. Moving intent into the
controller's revision log is a follow-on once this ships.

### Controller slices (`gsp-controller`, structural config distribution)
1. ✅ Skeleton binary + `store::Store` — Tier-1 store as an embedded KV
   (`sled`, ADR 20), single node, no Raft/etcd for this release. `open` /
   `current_revision` / `get` / `current` / `put`; `put` assigns the next
   monotonic revision and persists it + the `current` pointer in one `sled`
   transaction, then flushes. `gsp-controller` binary opens the store and
   serves `GET /healthz` (same observability floor as `gsp`). Nothing calls
   `POST /config` yet — that's slice 2.
2. ✅ `POST /config`: runs the **same `gsp_config::parse_str`** (parse +
   `validate()`) a proxy runs on a file reload, then `Store::put`. Rejects
   (`422`, error body) and leaves the current revision untouched on an
   invalid submission. `GET /config` returns the current revision's raw text
   + an `X-Config-Revision` header (`404` before the first submission).
3. ✅ `GET /config/subscribe?since=<revision>`: SSE. Sends the catch-up range
   (`Store::revisions_after(since)`) then tails a `broadcast::Sender<u64>`
   fed by `submit_config`; a lagging subscriber (a `RecvError::Lagged`) just
   re-runs the catch-up query from wherever it left off — `Store` never
   forgets a revision, so there is no delivery state on the writer side
   (`docs/10` principle 5). 5 new tests, incl. one that forces a lag and
   confirms no revision is skipped.
4. ✅ `gsp` gains `--controller <url>` (mutually exclusive with `--config` via
   clap `conflicts_with`). `controller_client::fetch_current` does the
   initial `GET /config` (mirrors `gsp_config::load` in file mode);
   `controller_client::run` then holds the subscribe connection and feeds
   every accepted revision through a new shared `reload::apply_config` (the
   rebuild/reconcile tail `reload::apply` and the controller path both call —
   only how the `Config` was obtained differs). On disconnect: reconnect with
   capped exponential backoff (500 ms → 30 s) from the last-applied cursor;
   the last-applied snapshot keeps running the whole time
   (freeze-on-last-known-good, never clear — verified live: killing the
   controller mid-session left the proxy serving traffic on its last config
   while retrying). An invalid pushed revision is logged and skipped (cursor
   still advances — it must not be replayed forever on every reconnect), the
   same "bad reload keeps the old snapshot" rule as a file reload.
5. ✅ `GET /config/revisions` (history: revision, size, `current` flag),
   `GET /config/revisions/{revision}` (a past revision's raw text),
   `GET /config/revisions/{revision}/diff[?against=<revision>]` (a
   `similar`-crate line diff against `current` or another revision, `+`/`-`/
   ` `-prefixed plain text), `POST /config/rollback/{revision}` (**never
   rewrites history** — re-submits that revision's bytes through the same
   validate-then-`Store::put` path as `POST /config`, so it's just an
   ordinary new revision to every subscriber, no special-casing anywhere
   else). `--auth-token <token>` on `gsp-controller` gates the whole
   `/config*` surface with a bearer-token check (`/healthz` stays open); a
   shared secret, not RBAC — appropriate for this release's one-controller
   scope. 8 new tests; verified live end-to-end over real HTTP (401 without
   the token, history/diff/rollback all round-tripped against
   `config.example.yaml`).

### Aggregator slices (`gsp-aggregator`, fleet view + operational verbs)
6. ✅ Skeleton binary (new crate, no `gsp-core`/`gsp-config` dependency — the
   aggregator stays fully decoupled from the data-plane crates) +
   `POST /ingest`. `ingest::IngestStore`: an in-memory, latest-write-wins
   map keyed by self-reported `instance` — deliberately never persisted,
   see the "stateless and ephemeral by design" note in the crate's `lib.rs`
   (an aggregator restart loses nothing that isn't about to be re-pushed on
   the next tick). Payload is a summary (pool/backend health + admin state,
   session *counts*), not the full live session registry — that already
   exists per-instance (`GET /sessions`), break-glass style. 12 new tests;
   verified live over real HTTP.
7. ✅ `gsp` gains `--aggregator <url>` (+ `--aggregator-instance`,
   `--aggregator-interval-sec`, default 10s) — independent of `--controller`,
   pushing state and pulling config are unrelated axes. `aggregator_client`
   builds an `IngestPayload` straight from the live `RuntimeHandle`
   (`snapshot().pools` for pool/backend summaries, `sessions()` filtered by
   `Proto` for TCP/UDP counts) and `POST`s it every tick; the wire shape is
   duplicated in `gsp` rather than adding `gsp-aggregator` as a dependency
   (same reasoning as `controller_client` hand-parsing the controller's SSE
   JSON instead of depending on `gsp-controller`). **No retry buffer** — a
   deliberate deviation from this line's original wording: `IngestStore` is
   latest-write-wins state, not an event log, so replaying an old failed push
   after a fresher one already landed would make the aggregator's view
   *older*; a failed push just logs and is superseded by the next tick's
   fresher snapshot. (Push, not pull — see "Fleet topology" in `docs/10` for
   why: no inbound network path to a proxy's admin port is ever needed, at
   any deployment size.) 1 new test (`build_payload` against a real
   `Runtime`); verified live end-to-end over real HTTP (`GSP_LOG=debug`
   showed two successful pushes 2s apart, matching `--aggregator-interval-sec
   2`).
8. ✅ `GET /fleet/pools` / `/fleet/sessions` / `/fleet/healthz` — served
   directly from `IngestStore` (no fan-out RPC needed, the data already
   arrived), each entry carrying `last_seen_ms_ago` (the aggregator's own
   clock). `/fleet/healthz` flags an instance `stale` past 30s (~3x `gsp`'s
   default push interval) — the threshold `IngestStore` itself deliberately
   doesn't have. **No `GET /fleet/config`** in this release: `IngestPayload`
   is state, not config content — the controller already owns that
   (`GET /config`/`/config/revisions`). A fleet-wide "which revision is each
   instance running" view is real and useful (an optional `config_revision`
   field on `IngestPayload`) but needs `controller_client` and
   `aggregator_client` to share state inside `gsp` that today are
   deliberately independent — deferred, not dropped. 8 new tests (incl. a
   test-only `IngestStore::insert_state` seam to test the staleness
   threshold without a real 30s wait); verified live end-to-end over real
   HTTP.
9. ✅ Intent-verb fan-out (`gsp-aggregator/src/fanout.rs`), thin and
   stateless — the aggregator decides nothing, stores no intent, just relays
   using each instance's self-reported `admin_url` (a new `IngestPayload`
   field, `http://{settings.admin.listen}`). Two shapes: **targeted**
   (`POST /fleet/instances/{instance}/drain`|`undrain` — draining *an*
   instance only ever means one instance, so this passes that instance's own
   response straight through, `404` for an unknown name) and **broadcast**
   (`POST /fleet/pools/{pool}/backends`, `PATCH`/`DELETE
   .../backends/{addr}`, `POST /fleet/route-hint` — no shared owner across
   instances this release, so applying fleet-wide means calling every known
   instance's own admin API independently via a `tokio::task::JoinSet`,
   reporting **per-instance results**; one bad instance never fails the
   others). 9 new tests using real mock instance HTTP servers (reachable,
   unreachable, and a real `Json` extractor to catch content-negotiation
   bugs) — one of them (`a_broadcast_sets_content_type_even_if_the_caller_
   never_did`) is a regression test for a real bug the live smoke test
   caught: the broadcast forwarded a caller's body without ever setting
   `Content-Type`, so a caller that omitted it got `415` from the target's
   own `Json` extractor even though the body was valid JSON — fixed by
   setting `application/json` explicitly on every forwarded body, since
   these endpoints are always JSON regardless of what the caller remembered
   to send. Verified live end-to-end over real HTTP (targeted drain flipped
   the real instance's `/readyz` to `503`; broadcast add-backend landed on
   the instance's real `/pools`).
10. Bearer-token auth on the aggregator's API; this also closes the "admin API
    has zero auth" gap on the proxy side (`settings.admin.auth_token`,
    checked by `admin.rs`, required by both the controller and the
    aggregator's per-instance calls).

### Web UI + tests
11. Static SPA: fleet dashboard + pool/backend table + operational actions
    (from the aggregator), config editor + revision history (from the
    controller). No proxy admin port ever exposed to a human directly.
12. Integration tests: N `gsp` instances + 1 controller + 1 aggregator —
    subscribe/reconnect/freeze-on-disconnect, push/ingest, fan-out partial
    failure, config reject-keeps-previous.
13. Docs: `docs/06` (new metrics/endpoints), `README.md` status block,
    `docs/08` status legend, `HANDOVER.md`.

- **Result**: one controller to distribute config to a fleet, persistently and
  with an audit trail, and one aggregator to view and operate that fleet from
  one screen — a real, working v2 release. The `standalone`/`slave`
  hierarchy, intra-tier HA, adoption, and moving intent into the controller
  are the next release on top of this, not blocking it.

## Phase 12 – Fleet hierarchy, HA, and shared intent
Full design: [10-distributed-control-plane.md](10-distributed-control-plane.md)
("Fleet topology", "Adoption"). Builds on phase 10+11's single-tier PoC —
additive, no rework of what shipped there.
- `standalone` / `slave` role per controller and aggregator tier (static,
  install-time, never inferred from connectivity) so a deployment can nest
  regions under a root tier.
- Intra-tier HA: a Raft/etcd-backed multi-replica group per tier (controller)
  and a horizontally-replicated stateless group per tier (aggregator) — an
  orthogonal setting from the role, per-tier.
- Operator intent (backend overlay, admin state, route hints, resolver pins)
  moves into the controller's revision log, fleet-wide and persisted across
  restarts; the phase-5 admin verbs become "controller writes a revision",
  direct per-instance admin stays as break-glass.
- Staged / canary rollout (a subset of instances/tiers take a revision
  first). RBAC on the controller/aggregator APIs.
- Adoption flow: an admin-UI action that flips a running `standalone` tier to
  `slave` under a newly-configured parent, reconciling its revision history
  and subtree.
- **Result**: the design in `docs/10` fully realized — config/intent
  consistent and durable across an arbitrarily large, regionally structured
  fleet, with no single point of failure at any tier.

## Phase 13 – Regional health fabric
Full design: [10-distributed-control-plane.md](10-distributed-control-plane.md)
(Tier 2). Advisory, rebuildable, off the data path.
- `failure_domain` / `region` identity per instance (`settings`, or discovered)
  — a reachability-equivalence class, not a building.
- A gossip / anti-entropy mesh among the instances in one domain (crate TBD —
  `foca` / SWIM, or a hand-rolled `(instance, backend)` LWW CRDT),
  authenticated (mTLS mesh / signed messages).
- Each instance publishes its per-backend `up | down`; consumes the domain view.
- Health decision becomes quorum-weighted: **unhealthy** on local `fall` **or**
  domain quorum-down; **healthy** only on local `rise`; Tier-1 `force-down`
  overrides.
- Metrics: per-backend domain agreement, fabric membership, gossip rate.
- Cold start / total partition ⇒ identical to today (own checks only).
- **Result**: faster, multi-vantage-point backend health across a domain; one
  bad vantage point no longer flaps a pool.

## Later / optional
- QUIC-CID-aware sniffer & session keying.
- Cross-instance session handover (shared *session* state) — still out of scope;
  the phase 10–13 control plane shares config and health, never sessions.
- eBPF/XDP pre-filter to drop floods before user space.
- Optional TLS/DTLS wrapping (proxy terminates, backend plain).

## Milestone cuts
- **MVP**: phase 0–2 (L4 TCP+UDP, static, health, metrics).
- **v1.0**: + phase 3–5 (routing, resolver, zero-downtime).
- **v1.1**: + phase 6–7 (client IP, hardening).
- **v1.2**: + phase 8 (discovery, HA operations docs).
- **v1.3**: + phase 9–10+11 (sniffer plugin loader; single-controller +
  single-aggregator PoC — fleet config distribution & operational Web
  UI). Both additive — the data-plane contract is unchanged.
- **v2.0**: + phase 12–13 (fleet hierarchy / HA / shared intent; regional
  health fabric) — the multi-region, no-single-point-of-failure realization of
  the phase 10+11 PoC — see
  [10-distributed-control-plane.md](10-distributed-control-plane.md).
