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
- ✅ k8s reads `GET .../endpoints/<svc>` with the in-pod SA token + CA, and
  (post-phase-8) **watches** it from the fetched `resourceVersion`: a pod change
  triggers a fetch at once, while the poll interval stays as the resync safety net.
- ✅ HA operations chapter in `docs/06` — anycast vs. L4 LB, capacity planning
  per instance, dashboards & alerts.
- **Result**: dynamic backend fleets, horizontal scaling.
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
  per-source cap LRU eviction — see
  `HANDOVER.md`. (`GET /sessions` since landed — data-plane completion.)
- ✅ **Post-phase-9 (data-plane completion)**: per-plugin config. ADR 16a
  widens the guest ABI to
  `sniff(in_ptr, in_len, cfg_ptr, cfg_len)`; `settings.sniffers.modules[]`
  grows a `config` string, marshalled into a second linear-memory region on
  every call. `a2s` / `minecraft` ignore it; `regex-firstbytes` was rebuilt
  around it — a tiny `[key:NAME|] [@OFFSET ](hex:…|ascii:…){|…}` pattern
  language, so it is a genuinely runtime-configured bounded matcher and not the
  hard-coded HTTP template.

## Phase 10+11 (merged) – PoC: single controller + single aggregator ✅
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
   `--controller-token` (added later, slice 11f testing) is what `gsp`
   presents to a `--auth-token`-protected controller — every other
   cross-service link in this fleet has a token pairing, and this one was
   missing until driving the real UI against a real fleet surfaced it.
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
10. ✅ Bearer-token auth, three independent secrets for three independent
    hops (never one token threaded through everything): `gsp-aggregator
    --auth-token` gates its own API (`/ingest`, `/fleet/*`, fan-out writes;
    `/healthz` stays open); `settings.admin.auth_token` (new `gsp-config`
    field) closes the "admin API has zero auth" gap on `gsp`'s own admin API
    the same way (`admin.rs` gains a `require_bearer` middleware, mirroring
    `gsp-controller`'s); `gsp --aggregator-token` is what a proxy presents
    pushing to `/ingest`; `gsp-aggregator --instance-token` is the separate
    secret the aggregator presents *out* to every instance's admin API when
    fanning out (`settings.admin.auth_token` on the instance side must
    match). 3 new tests (admin auth gate, aggregator auth gate, `gsp-config`
    parsing `auth_token`). Verified live end-to-end over real HTTP through
    every hop: unauthenticated aggregator read → `401`; authenticated read
    showed the real pushed data; unauthenticated instance admin call → `401`;
    a fan-out drain (through the aggregator, correctly presenting
    `--instance-token`) → `200`, confirmed real by the instance's own
    `/readyz` flipping to `503`.

**All 10 slices of the phase 10+11 PoC's controller + aggregator halves are
now done** — the remaining slices (11–13) are the Web UI, integration tests,
and docs polish.

### Web UI + tests

Slice 11 is a **dedicated `gsp-ui` process — a BFF (backend-for-frontend),
not a static SPA calling the controller/aggregator bearer-token APIs
directly.** Design refinement made when actually starting it, now locked in
`docs/10` ("The admin GUI"): the browser gets its own session-cookie login
on `gsp-ui`, wholly separate from every machine-to-machine bearer token
(`--auth-token`/`--instance-token`/`--aggregator-token`/
`settings.admin.auth_token`), none of which a browser ever holds — `gsp-ui`
holds the controller's and the aggregator's tokens itself and calls both on
the operator's behalf. `gsp-ui` has no store and no fleet data of its own
(nothing outlives a restart beyond active sessions) — it is not a third
authority, it authorizes nothing itself beyond "is this a valid session." A
React + Vite + TypeScript SPA it serves itself (`tower-http::ServeDir`), one
process, one port for the operator. No proxy admin port, and no machine
token, is ever exposed to a human directly.

11a. ✅ `gsp-aggregator` gains `GET /fleet/subscribe` (SSE, bearer-gated like
     the rest of `/fleet/*`) — the machine-to-machine feed `gsp-ui` (11d)
     subscribes to for live updates: current merged fleet state
     (`FleetInstanceView` — pools + sessions + a `stale` flag, one entry per
     instance) on connect, then a resend, debounced 150 ms against a burst,
     on every accepted `POST /ingest`. Reuses the exact
     catch-up-then-broadcast shape `gsp-controller`'s `/config/subscribe`
     already proved out, applied to *state* instead of a revision log — no
     cursor/replay semantics needed, a `Lagged` subscriber just means
     "rebuild now," same as any other signal. 3 new tests (immediate initial
     send, resend-after-debounce, a 5-push burst collapsing into exactly one
     resend); verified live over real HTTP against a real `gsp` push loop.
11b. ✅ New crate `gsp-ui` (lib + bin, no `gsp-core`/`gsp-config` dependency —
     stays as decoupled as `gsp-aggregator` is). `--ui-password`:
     `POST /ui/login {password}` issues a random 256-bit session id
     (`session::SessionStore`, in-memory — a restart just logs everyone out,
     the same "ephemeral, nothing durable" posture the aggregator already
     has), returned as an `HttpOnly`, `SameSite=Lax` cookie (not yet marked
     `Secure` — noted as a gap for a TLS-fronted deployment, not silently
     ignored); `POST /ui/logout` clears it; `GET /ui/session` (gated by the
     new `require_session` middleware) lets the frontend check login state
     on load. `None` (`--ui-password` omitted) leaves the UI open, consistent
     with every other optional-auth surface in this fleet. 9 new tests;
     verified live end-to-end over real HTTP through the whole cycle:
     unauthenticated → `401`, wrong password → `401`, right password → a
     cookie that unlocks the gated route, logout → `401` again.
11c. ✅ `gsp-ui` proxies reads + slice-9 operational verbs to
     `gsp-aggregator` (`--aggregator-url`/`--aggregator-token`) — a new
     `aggregator_proxy.rs`, thin and stateless like `gsp-aggregator::fanout`
     it calls through to: `GET /api/fleet/pools`/`sessions`/`healthz`,
     `POST /api/fleet/instances/{instance}/drain`|`undrain`, backend
     add/patch/delete, route-hint. Translates the browser's session cookie
     into the aggregator's bearer token server-side — the browser never
     holds it. `503` if no `--aggregator-url` was configured at all, `502`
     if it was but couldn't be reached, both handled cleanly (no panic). This
     is phase 10's "operational" GUI capability level (`docs/10`):
     read-heavy plus these verbs, no structural config editing yet. 6 new
     tests (incl. a real mock-aggregator round-trip proving the token and
     request body are forwarded correctly). Verified live end-to-end over
     the real four-hop chain: unauthenticated browser call → `401`; logged
     in → `GET /api/fleet/pools` showed the real pushed data; a drain
     through the full `gsp-ui → gsp-aggregator → gsp` path landed for real,
     confirmed by the instance's own `/readyz` flipping to `503`.
11d. ✅ `gsp-ui`'s `GET /ws/fleet`: a WebSocket to the browser, fed by a
     single shared `crate::fleet_feed` subscription to `gsp-aggregator`'s
     slice-11a SSE feed (one aggregator connection total, fanned out to
     every browser tab — not one per tab). `fleet_feed::run` reuses `gsp`'s
     own `controller_client`'s hand-rolled SSE parsing (a chunked-body loop
     splitting on blank lines) and reconnects with the same capped
     exponential backoff on disconnect; a `latest` cache means a browser
     connecting between two pushes gets the current view immediately rather
     than waiting for the next one. A `Lagged` WS subscriber resends
     `latest` rather than replaying — same "this is state, not a log"
     reasoning as `gsp-aggregator`'s own `subscribe_fleet_worker`. Gated by
     `require_session` like everything else the browser reaches (a WS
     upgrade is an ordinary `GET` until the `101` handshake). 5 new tests,
     3 of them against a *real* WebSocket client (`tokio-tungstenite`) and a
     real `axum::serve` listener, not mocks. Verified live end-to-end over
     the full five-hop chain with a throwaway probe client: real `gsp` →
     real `gsp-aggregator` (SSE) → real `gsp-ui` (`fleet_feed`) → a
     WebSocket client, watching live pushes arrive in real time.
11e. ✅ `gsp-ui` proxies the controller's config API too
     (`--controller-url`/`--controller-token`, new `controller_proxy.rs`):
     `GET`/`POST /api/config`, `GET /api/config/revisions(+/{rev}(/diff))`,
     `POST /api/config/rollback/{rev}` — phase 10's "full management" GUI
     level, straightforward since `gsp-ui` already holds a separate token
     per service. The controller's `POST /config` body is raw YAML text
     (its handler takes a plain `String`, not JSON), forwarded byte-for-byte
     with no content-type forced on it, unlike the JSON bodies
     `aggregator_proxy` forwards. The diff route forwards the incoming
     query string (`?against=`) via `axum::extract::RawQuery` onto the
     proxied URL. 6 new tests. **Found and fixed a real bug along the way**:
     both proxy helpers (here and in `aggregator_proxy`, plus
     `gsp-aggregator`'s own `fanout::proxy_to_instance`) forwarded only
     status + body, silently dropping every response header — caught
     immediately by a test asserting `X-Config-Revision` survived the hop,
     which it didn't. Fixed with a shared `proxy_util::forwardable_headers`
     (strips only the hop-by-hop headers `connection`/`transfer-encoding`/
     `content-length`, forwards everything else verbatim) and a matching
     fix + regression test in `gsp-aggregator`. Verified live end-to-end
     over the full chain against a real `gsp-controller`: submit → list
     revisions → diff → rollback → `GET /api/config` correctly showing
     `X-Config-Revision: 3` (the new post-rollback revision, not a rewind).
11f. ✅ Frontend: React + Vite + TypeScript in `crates/gsp-ui/web/` (own
     `package.json`, never a Cargo workspace member — same reasoning as
     `crates/plugins/`; `make ui` runs `npm install && npm run build`,
     output goes to `dist/`). `gsp-ui` gained `--static-dir` (default
     `crates/gsp-ui/web/dist`), served as a fallback under whatever the API
     routes don't claim, via `tower-http::services::ServeDir` (new
     dependency). No client-side routing — one page, view state (which tab)
     lives in React state, not the URL; nothing here needs a deep link yet.
     `Login` (session-cookie form), `FleetView` (per-instance/pool/backend
     table live over 11d's WebSocket, drain/undrain, backend add/patch/
     remove, route-hint — phase 10's "operational" level), `ConfigView`
     (editor + submit + revision history/diff/rollback — phase 10's "full
     management" level). `src/api.ts` holds every `fetch` call; `gsp-ui`'s
     session cookie is the only credential the browser ever sends — never a
     bearer token, per the whole point of the `gsp-ui` redesign. Verified
     live end-to-end with the **real built frontend** served by a **real
     four-process fleet** (`gsp` + `gsp-controller` + `gsp-aggregator` +
     `gsp-ui`, all running together for the first time): `index.html` and a
     JS asset served with correct content-type, login, a config submission
     and a fleet-pools read through the served UI's own proxy paths, and a
     drain issued through the full chain landing for real (confirmed by the
     instance's own `/readyz` flipping to `503`).

- **Slice 11 (all of 11a-11f) is now complete.** The whole `gsp-ui` BFF —
  session login, fleet reads/ops proxying, the live WebSocket, config
  editing/history proxying, and the actual frontend serving all of it — has
  been verified live end-to-end, repeatedly, against real running
  `gsp`/`gsp-controller`/`gsp-aggregator` processes, not just against unit
  tests.
- **The 11f frontend itself was a functional PoC, not a finished operator
  UI** (confirmed by the user actually clicking through it in a browser; since
  redesigned — see "Later / optional" below) —
  every backend path it drives is real and solid, but the UI layer is
  deliberately bare: no styling/design pass, no confirmation dialogs before
  a destructive action (drain, remove-backend fire immediately), no loading
  states beyond a bare "loading…", no client-side routing, plain unstyled
  tables and forms. **A real frontend overhaul is future, separate work** —
  tracked under "Later / optional" below — deliberately deferred rather than
  polished now, since it doesn't block anything else in this phase.
12. ✅ Integration tests: N `gsp` instances + 1 controller + 1 aggregator —
    subscribe/reconnect/freeze-on-disconnect, push/ingest, fan-out partial
    failure, config reject-keeps-previous. New crate `crates/gsp-fleet-tests`
    (workspace member, part of `make check`): spawns the real
    `gsp`/`gsp-controller`/`gsp-aggregator` **binaries** as child processes on
    loopback with OS-assigned ports and drives them over real HTTP, rather
    than an in-process harness — matching how every earlier slice was
    actually verified (see HANDOVER.md) and specifically able to catch the
    wire-shape class of bug slices 11e/11f already found live. 4 tests, all
    green. `gsp-ui` isn't spawned here — nothing in the 4 listed scenarios
    exercises it, and it has no state of its own to assert on beyond what
    slice 11's own tests already cover.
13. ✅ Docs: `docs/06` gained a "Fleet control plane" section (full endpoint
    reference for `gsp-controller`/`gsp-aggregator`/`gsp-ui`, and updated the
    now-stale "not persisted or fleet-synced today" / "no built-in fleet-wide
    view" callouts in "Multi-instance operations" to point at it); `README.md`
    status block now covers phase 10+11; this status legend; `HANDOVER.md`.

- **Result**: one controller to distribute config to a fleet, persistently and
  with an audit trail, and one aggregator to view and operate that fleet from
  one screen — a real, working v2 release. The `standalone`/`slave`
  hierarchy, intra-tier HA, adoption, and moving intent into the controller
  are the next release on top of this, not blocking it.
- **All 13 slices of phase 10+11 are now complete.**

## Phase 12 – Fleet hierarchy, HA, and shared intent ✅
Full design: [10-distributed-control-plane.md](10-distributed-control-plane.md)
("Fleet topology", "Adoption"). Builds on phase 10+11's single-tier PoC —
additive, no rework of what shipped there.
- ✅ **Slice 1 (`gsp-controller` role)**: `standalone` / `slave` role, static
  and install-time (`--role`, never inferred from connectivity). A
  `standalone` tier is unchanged from phase 10+11. A `slave` tier
  (`--parent-url` + `--parent-token`) never accepts a write directly
  (`POST /config` and `/config/rollback/*` both `403`) — instead
  `parent_client` seeds from the parent's `GET /config` at startup, then
  subscribes to `GET /config/subscribe` and relays every accepted revision
  into its own store (its own local revision numbers, not the parent's —
  only the payload shape is shared). Reuses `gsp`'s own
  `controller_client`'s reconnect-with-backoff shape; a lost parent freezes
  the slave on last-known-good and keeps serving/relaying it downward, same
  "never clear" rule as the proxy-to-controller hop. Verified live:
  root submits a revision → slave relays and serves it with its own
  `X-Config-Revision`; a direct write to the slave is `403`; killing the
  root leaves the slave serving what it had and retrying with backoff.
  **Not yet built at the time**: aggregator-side hierarchy, intra-tier HA
  (Raft/etcd), intent migration into the revision log, RBAC, canary
  rollout, adoption.
- ✅ **Slice 2 (`gsp-aggregator` hierarchy)**: `--parent-url` (+
  `--tier-name`, `--parent-token`, `--parent-push-interval-sec`) makes a
  `gsp-aggregator` tier also push its own merged view up to a parent
  aggregator's `POST /ingest`, on a fixed interval, via new
  `parent_push.rs` — "a tier's aggregator is itself a valid leaf to its
  parent aggregator" (`docs/10`). Deliberately namespaces rather than
  merges: every instance this tier currently knows is pushed individually
  with its `instance` renamed `"{tier_name}/{instance}"`, so the parent's
  `IngestStore`/`/fleet/*` need no new payload shape or special-casing —
  it just sees more, longer-named instances (namespacing only touches the
  copy sent upward; this tier's own local names and `/fleet/*` view are
  untouched). 3 new tests, incl. one against a real mock parent
  `axum::serve` listener. Verified live: a child aggregator ingesting
  `proxy-1` shows up on a real parent aggregator's `GET /fleet/sessions`
  as `region-a/proxy-1`.
  **Not yet built at the time**: intra-tier HA, intent migration, RBAC,
  canary rollout, adoption.
- ✅ **Slice 3 (operator intent → the controller's revision log)**:
  pool-scoped operator intent — backend add/remove, backend admin state,
  route-hint — now has a fleet-wide, persisted path through
  `gsp-controller`, alongside (not replacing) direct per-instance admin API
  calls. New `gsp-controller::intent` module: a second `sled` log (its own
  `<data_dir>/intent` database, reusing `Store` unchanged), `POST /intent`
  (validates the op's shape — addr/IP parse, known state — before it's ever
  broadcast, since there's no `gsp_config::validate()` to lean on for a bare
  op) and `GET /intent/subscribe`, the identical catch-up-then-tail shape
  config uses. **Deliberately excludes** whole-instance drain/undrain (it
  targets one instance, not "every instance with pool X" — stays on the
  aggregator/direct-admin path) and resolver pins (still an open question,
  `docs/01`). New `gsp` module `intent_client.rs` subscribes whenever
  `--controller` is set and applies each op through the exact same
  `RuntimeHandle` calls `crate::admin`'s handlers make — an intent op is a
  new *source* for an existing mutation, not a new code path. 15 new tests
  across both crates.
  **Real pre-existing bug found and fixed along the way, via this slice's
  live smoke test**: in `--controller` mode, nothing was watching
  `RuntimeHandle::reload_requested()` at all — `reload::run` is the only
  thing that ever did, and it's only spawned in file-config mode — so the
  phase-5 admin API's own `POST`/`DELETE /pools/{pool}/backends` (backend
  overlay edits) silently never took effect for any `--controller` instance,
  before this slice existed. Fixed with a new
  `controller_client::watch_admin_reloads`, spawned alongside
  `--controller`: debounces `request_reload()` notifications and re-fetches
  + re-applies the controller's current config (rather than caching a local
  copy, so there's nothing to drift from the controller's own view).
  Verified live end-to-end: `gsp-controller` + `gsp --controller` running
  for real, a `POST /intent` `backend_add` landed in `gsp`'s own
  `GET /pools` output within one debounce window.
  **Not yet built at the time**: intra-tier HA, slave-tier intent relay
  (only config relayed through a parent then), RBAC, canary rollout,
  adoption.
- ✅ **Slice 4 (slave-tier intent relay)**: the intent-log counterpart to
  slice 1's config relay. New `gsp-controller::intent::relay`, structurally
  identical to `parent_client` (subscribe-with-backoff, land via
  `IntentState::apply_revision` — the same role-gate bypass) with one
  difference: no `fetch_initial` seed step, since an intent log has no
  single "current document" to seed from — `since=0`'s catch-up on the
  first subscribe already replays the parent's whole history, unlike
  config's "must serve *something* the moment a fresh slave comes up." A
  `slave` tier now relays *both* logs from its parent, still never
  originating either directly (`403` on both `/config` and `/intent`
  writes). 4 new tests. Verified live with two real `gsp-controller`
  processes: root submits an intent op → slave relays and serves it under
  its own local revision number; direct writes to the slave still `403`;
  killing the root freezes the slave on both logs, retrying with backoff.
  **Not yet built at the time**: intra-tier HA, RBAC, canary rollout,
  adoption.
- ✅ **Slice 5 (adoption)**: `POST /admin/adopt {"parent_url":...}` flips a
  running `standalone` tier to `slave` without a restart (`docs/10`
  "Adoption"). Solves the design doc's two open correctness questions the
  same simple way: adoption is refused (`409`) unless **both the config and
  intent stores are still empty** — a tier with its own revision history
  must be replaced by a fresh one to join a hierarchy, not reconciled in
  place; an empty store trivially has nothing in flight to reorder either.
  New shared `role::RoleHandle` (an `Arc<RwLock<Role>>`, replacing the
  plain `Role` field `AppState`/`IntentState` held) so the flip is visible
  to both write gates instantly, from every clone. On success: seeds from
  the new parent's config (mirrors `main.rs`'s own boot-time seed) and
  spawns the same `parent_client`/`intent::relay` tasks a `--role slave`
  boot would have — a freshly-adopted tier is indistinguishable from one
  that started as a slave. 6 new tests. Verified live with two real
  `gsp-controller` processes: adopting a fresh child returned
  `{"seeded_config_revision":1}`, its `GET /config` immediately served the
  root's config, and a direct write to it was `403` from that point on.
  **Still not built**: intra-tier HA, RBAC, canary rollout.

**Slices 6–8 (2026-09-05 design session, now all built)** — each has its own
`docs/10` section (with wire shapes, storage layout, and rejected
alternatives) and a `docs/09` ADR (21–23):

- ✅ **Slice 6 — Intra-tier HA** (`gsp-controller` only; the aggregator's HA
  design — stateless replicas behind one address, no consensus needed —
  stays design-only, not built): embedded `openraft` 0.9, one Raft group
  per controller tier replicating both the config and intent logs. New
  `gsp_controller::ha` module: a `sled`-backed `LogStore`
  (`RaftLogStorage`/`RaftLogReader`, its own `<data_dir>/ha` database —
  durable across a restart, unlike `openraft`'s own in-memory reference
  implementation this was adapted from), a `StateMachineStore`
  (`RaftStateMachine`/`RaftSnapshotBuilder`) that applies a committed entry
  by calling the *exact same* `AppState`/`IntentState::apply_revision` a
  direct write already used, a `reqwest`-based `RaftNetwork` posting to
  peers' `/raft/*` routes (gated by a new peer-only `--ha-token`), and
  `ha::client::propose_write` — the one call `api::submit`/
  `intent::api::submit_intent` make instead of `apply_revision` directly
  when `--ha-peers` is set: proposes via Raft, and **transparently
  HTTP-forwards to the current leader** (never a redirect) if this replica
  isn't it, so no client anywhere needs to know HA exists. New CLI:
  `--ha-node-id`, `--ha-peers id=host:port,...` (identical on every
  replica; each boots and calls `raft.initialize()` with the same static
  set — harmless no-op on every node but the one that wins the race),
  `--ha-token`. **Scope cut, stated in the module doc**: HA and the `slave`
  role are mutually exclusive in this slice (enforced at startup) —
  combining them needs the upward relay to run leader-only with a
  replicated cursor, designed in `docs/10` but not built here. 16 new
  tests. **Verified live with a real 3-node cluster**: booted nodes 1/2/3,
  confirmed node 3 elected leader; submitted a config write to node 1 (not
  the leader) — transparently forwarded, `200`, `{"revision":1}`; submitted
  to node 2 — `{"revision":2}`; all three nodes' `GET /config` agreed on
  revision 2. Then **killed the leader (node 3)** and confirmed real
  failover: node 1 was elected the new leader within seconds, and both a
  config write (revision 3) and an intent write both succeeded through the
  surviving 2-node majority.
- ✅ **Slice 7 — Staged / canary rollout**: one monotonic config log, never
  forked — revisions tagged `{promoted, canary_groups}` in a new `stage`
  `sled` tree (a sibling of `Store`'s own trees, opened via a new
  `Store::db()` accessor; `Store` itself gains nothing, stays exactly the
  content-agnostic log it always was). `POST /config?stage=canary&group=
  <name>` submits a revision visible only to a subscriber reporting that
  group (`GET /config?group=<name>`, `GET /config/subscribe?group=
  <name>`); a plain `POST`/`GET` with no `stage`/`group` is byte-for-byte
  the pre-slice-7 behavior (every revision defaults `Stage::promoted`).
  `POST /config/promote/{revision}` is the one deliberate exception to
  "every change is a new revision" — flips an existing revision's
  visibility in place. `GET /config/revisions` gained `promoted`/
  `canary_groups` columns. `subscribe_worker`'s catch-up/tail now filter
  every candidate through `Stage::visible_to`, holding an invisible
  revision back (without advancing the subscriber's cursor past it) until
  a later signal — its own promotion, or any newer submission — makes it
  either visible or moot. HA-aware: `WriteRequest::Config` now carries its
  `Stage` through the Raft log, and a new `WriteRequest::Promote(revision)`
  variant replicates a promotion the same way. **Deliberately config-only,
  not the intent log** — no clean partial-rollout meaning for a single
  already-narrow op (see `docs/10`). 7 new tests. Verified live: submitted
  a promoted v1 and a `region-a`-canary v2; a plain `GET /config` and
  `GET /config?group=region-b` both still showed v1; `GET /config?group=
  region-a` showed v2; `POST /config/promote/2` then made v2 the plain
  `GET /config` answer too. See `docs/10` "Staged / canary rollout
  (design)", ADR 22.
- ✅ **Slice 8 — RBAC and audit**: multi-operator accounts (`--users-file`,
  `argon2` PHC hashes, `gsp-ui --hash-password` to produce one) and three
  roles (`viewer`/`operator`/`admin`) enforced in `gsp-ui` — the
  controller/aggregator keep their existing single shared-token gates
  unchanged, per the design's explicit continuation of the phase 10+11
  slice-11 divergence. `--ui-password` is kept (not replaced) as a legacy
  single-shared-secret, implicitly-`admin` mode. New `gsp_ui::role::Role`
  (`Viewer < Operator < Admin`, derived `Ord`) and a `SessionStore` that now
  maps a session id to `{role, username}`; `crate::auth::check_role(min)`
  gates three separately-`route_layer`ed sub-routers (`viewer`/`operator`
  from `aggregator_proxy`, `viewer`/`admin` from `controller_proxy`) — `401`
  for no/invalid session, a new `403` for a valid session below the route's
  minimum. A new `X-Actor` header (the session's username, if any) rides
  every write `gsp-ui` proxies; `gsp-controller` records it per revision in
  a new `actors` `sled` tree (`GET /config/revisions` gained an `actor`
  field — through HA too: `WriteRequest::Config` now carries `actor`
  alongside `stage`, and leader-forwarding preserves the header so
  whichever node ends up proposing the write has it); `gsp-aggregator`
  forwards it to each instance and logs `(instance/pool, actor, verb)` via
  `tracing` (a convenience, not a durable record — this aggregator holds no
  durable state by design). 17 new tests across `gsp-ui`/`gsp-controller`/
  `gsp-aggregator`. Verified live: hashed a password with
  `--hash-password`, logged in as that user against a real `gsp-ui` with
  `--users-file`, submitted a config through the full `gsp-ui` → real
  `gsp-controller` chain, and confirmed `GET /config/revisions` showed
  `"actor":"alice"` on the resulting revision. See `docs/10` "RBAC and
  audit (design)", ADR 23.

**Phase 12 is now fully built** — all 8 slices (hierarchy, both relay
logs, adoption, intra-tier HA, staged/canary rollout, RBAC and audit) are
implemented, individually verified live, and covered by `make check`. The
design in `docs/10` is realized: config/intent consistent and durable
across an arbitrarily large, regionally structured fleet, no single point
of failure at any tier, and per-role accountability for every change.
Phase 13 (the regional health fabric, Tier 2) is now **fully built** — see
below.

## Phase 13 – Regional health fabric ✅
Full design: [10-distributed-control-plane.md](10-distributed-control-plane.md)
(Tier 2). Advisory, rebuildable, off the data path. **Fully built
(2026-09-05)** — all 5 slices below, verified live including two real
multi-process tests in `crates/gsp-fleet-tests`; see "Mechanism (built)" in
`docs/10` and ADR 24 in `docs/09` for the locked design this implements.
Mechanism: membership via embedded
`foca` (SWIM); per-backend health as a last-writer-wins `(instance,
backend)` register piggybacked on foca's own broadcast/anti-entropy;
HMAC-SHA256 over a per-domain pre-shared key, not mTLS.

Slices:

1. ✅ **Config schema** (done): `settings.failure_domain` +
   `settings.gossip {bind, seeds, quorum_fraction, psk}` in `gsp-config`
   (raw + resolved types, `validate()` rejects one without the other),
   `config.example.yaml`, `docs/05`.
2. ✅ **Membership** (done): new `gsp-core::gossip` module wrapping
   `foca::Foca` (SWIM) over a plain UDP socket, spawned by
   `Runtime::start_with_discovery` only when `settings.gossip` is set
   (`gsp/src/main.rs` passes `cfg.gossip.clone()`; startup-only, like
   `geo`/`sniffers` — a reload does not start or stop the mesh).
   HMAC-SHA256-tagged/authenticated datagrams (bad or missing tag ⇒ dropped
   silently + `gsp_gossip_auth_rejected_total`); `gsp_gossip_members` /
   `gsp_gossip_messages_total` in `metrics_defs.rs`. This slice's
   `NoCustomBroadcast` means no application payload rides the mesh yet — pure
   membership. 8 new tests (6 unit incl. two real two-process-equivalent
   in-tokio SWIM convergence tests; a wrong-PSK-never-joins test). Verified
   live with two real separate `gsp` processes on real sockets (not
   in-process): both converged to `gsp_gossip_members 1` with real
   send/receive traffic on `/metrics` within the SWIM probe period.
3. ✅ **Per-backend health broadcast** (done): `BackendHealthRegister {addr,
   up, changed_at, origin}` is a last-writer-wins payload piggybacked via a
   new `BroadcastMerger: foca::BroadcastHandler<SocketAddr>` — `foca`'s own
   anti-entropy re-gossips it until fully propagated, no second sync pass.
   `GossipHandle::publish_backend_health(addr, up)` (an `mpsc` channel into
   the mesh task; the actual `health.rs` caller is slice 4) asserts this
   instance's own verdict; `GossipHandle::domain_votes(addr)` /
   `quorum_down(addr, fraction)` read the merged view (`(0, 0)` votes ⇒
   `quorum_down` is always `false` — an empty view never overrides, only a
   real quorum can). 5 new tests: pure `domain_votes`/`quorum_down` unit
   tests, and a real two-process-equivalent in-tokio test proving a
   published register actually reaches the other instance through foca's
   own gossip (not a direct map share) and that `quorum_down` reads it back
   correctly. **One real bug found by that test, not by inspection**: an
   early draft merged the register into the domain map *before* calling
   `foca.add_broadcast`, which made `foca`'s own internal
   `receive_item(data, None)` call see an already-known key and silently
   skip queuing it for dissemination — fixed by letting `add_broadcast`
   perform the one and only merge (the `sender: None` case in
   `BroadcastHandler::receive_item` exists exactly for this "adding it
   myself" path).
4. ✅ **Pool/health integration** (done): `Backend` gained `domain_down:
   AtomicBool` alongside the existing `healthy` (untouched); `is_healthy()`
   = `local_healthy() && !domain_down_flag()`. New `Backend::
   observe_domain(quorum_down)` can only set/clear `domain_down`, never
   `healthy` — a backend still only returns healthy on this instance's own
   `rise` streak. `health.rs::sweep` gained an optional `GossipFabric`
   (handle + `quorum_fraction`, `None` when `settings.gossip` is unset):
   every active-check result also calls `GossipHandle::
   publish_backend_health(addr, ok)` (only for backends this sweep itself
   just probed — the "only ever asserts what it checks itself" rule from
   the design), and after every sweep, every backend's `domain_down` is
   refreshed from `GossipHandle::quorum_down`. `Runtime::start_with_discovery`
   builds one `GossipFabric` (if `settings.gossip` is set) and hands it to
   both the health task and the gossip task, so `sweep` never has to reach
   across tasks. `gsp_backend_domain_down{pool,backend}` gauge added. 5 new
   tests: 3 pure `pool.rs` unit tests (domain-down overrides but local
   `rise` alone clears it; domain-down clearing never revives a locally-down
   backend; `domain_down` carries across a reload by address, mirroring how
   `healthy` already does) + 2 `health.rs` tests, one proving `sweep` never
   touches `domain_down` with no gossip fabric, one driving a real
   single-node mesh through the full real pipeline (channel → mesh task →
   `add_broadcast` → merge) and confirming a repeated `sweep` eventually
   reads its own published verdict back and overrides the backend down.
5. ✅ **Verification** (done, `crates/gsp-fleet-tests/tests/gossip.rs`): two
   real multi-process tests, spawning actual `gsp` binaries the same way
   every other fleet slice was verified. `domain_quorum_overrides_an_
   instance_with_a_lenient_local_threshold` gives 3 real processes the
   *same* real (unreachable) backend but a per-instance-different `fall`
   threshold (2 instances `fall: 1`, one `fall: 1000`) — a genuinely mixed
   local view achieved with plain config, no faked wire data — and confirms
   the lenient instance's combined `is_healthy()` still goes false purely
   from the domain quorum, live proof of "an instance cannot ignore a
   domain-wide outage." `instances_with_different_psks_never_merge_and_
   neither_gets_stuck` confirms two real processes with mismatched PSKs
   never converge (`gsp_gossip_members` stays `0` on both) yet each still
   correctly runs its own local-only health checks and neither process
   hangs or crashes — the "fully rebuildable, falls back to local-only"
   property with a real empty mesh, not a config that merely looks empty.

- Each instance publishes its per-backend `up | down`; consumes the domain view.
- Health decision becomes quorum-weighted: **unhealthy** on local `fall` **or**
  domain quorum-down; **healthy** only on local `rise`; Tier-1 `force-down`
  overrides.
- Cold start / total partition ⇒ identical to today (own checks only).
- **Result**: faster, multi-vantage-point backend health across a domain; one
  bad vantage point no longer flaps a pool.

## Phase 14 – Backend transport ✅
Full design: [11-backend-transport.md](11-backend-transport.md). Closes the
gap `docs/01-requirements.md`'s Assumptions section states outright
("Backends are reachable over a trusted internal network") — not true once
proxy instances are distributed globally (phase 10–13's whole point) in
front of game servers on a different network entirely. **Complete — all 7
slices built**, see ADR 25 in `docs/09`.

Mechanism (design, locked 2026-09-05 follow-up session — see docs/11
"Locked decisions"): unmodified WireGuard via `defguard/wireguard-rs`
(unifies kernel-netlink and `boringtun`-userspace config behind one API) as
the tunnel data plane between an edge proxy and an origin — no new crypto/
framing protocol, and one shared interface per side with many peers, not
one interface per origin. `connect_backend`/`connect_upstream`/
`health.rs`'s dials need no changes, since a tunneled backend is just a
routable `SocketAddr` once the interface exists. An origin is modeled as a
new `BackendSource` (ADR 12a) — `backend_sources[].type: tunnel` — not a
new schema concept. The two genuinely new pieces: a small origin-side
`gsp-agent` crate that manages the local WireGuard interface and registers
its pubkey/backend addresses, and a "backend peers" registry on the
existing Tier-1 `gsp-controller` that distributes peer configuration to
every subscribed edge proxy the same way config revisions already are.
WireGuard's own roaming + keepalive mean only the proxy side ever needs a
public endpoint — an origin behind a home NAT needs no port forwarding.

Slices, ordered by dependency (each was buildable and testable before the
next needed it; all now built):

1. ✅ **Config schema**: `backend_sources[].type: tunnel` (`gsp-config` raw +
   resolved types, `validate()`, `config.example.yaml`, `docs/05`) —
   parses and validates with no runtime behavior behind it yet, same
   "schema first, mechanism after" shape phase 13 slice 1 used. Fields:
   at minimum the origin's expected public key and which pool(s) it feeds;
   exact shape decided in this slice, not locked in `docs/11`.
2. ✅ **`gsp-controller`'s backend-peers registry**: a new resource
   alongside the config-revision and intent logs (its own `sled` tree/DB,
   matching `Store`'s own precedent) — `POST` for an agent to register
   (pubkey, allowed backend addresses, last-known endpoint) and a
   subscribe endpoint mirroring `GET /config/subscribe`'s catch-up-then-
   tail shape. Buildable and independently testable with nothing but
   `curl`/the crate's own HTTP tests — no real WireGuard, `gsp-agent`, or
   `gsp` integration needed yet, same as how the config/intent logs were
   each built and tested standalone before any client integrated with
   them.
3. ✅ **New `gsp-agent` crate**: creates/maintains one local WireGuard
   interface via `defguard/wireguard-rs`, registers with slice 2's
   registry on startup and on change. Verifiable standalone: point a real
   `gsp-agent` at a real `gsp-controller`, confirm its registration lands
   and a real local WireGuard interface comes up with the right key.
4. ✅ **`gsp`'s subscribe-and-reconcile task**: mirrors
   `controller_client.rs`'s shape, subscribes to slice 2's registry, and
   reconciles the proxy's own shared WireGuard interface's peer list (via
   `wireguard-rs`) to match. Verifiable standalone against slice 3's real
   agent: the proxy's interface should show the agent as a peer within one
   subscribe cycle, independent of any actual game traffic yet.
5. ✅ **The new `tunnel` `BackendSource`** (`gsp` binary, same seam as the
   existing DNS-SRV/Consul/Kubernetes sources): `fetch()` resolves an
   origin's currently-registered backend address(es) from the same peers
   registry slice 4 already subscribes to. This is what actually lets a
   pool's `source: <origin>` produce live, tunnel-internal backend
   addresses through the existing discovery reconcile path — no changes
   needed to `Snapshot::build_with_sources` itself.
6. ✅ **End-to-end live verification**: a real `gsp-controller` +
   `gsp-agent` (+ a plain TCP echo backend) + `gsp` proxy in four Docker
   containers (`NET_ADMIN` + `/dev/net/tun` on the two WireGuard sides —
   this project's own dev sandbox has neither, so containers were the
   practical way to get a `CAP_NET_ADMIN`-capable host), a real payload
   round-tripped through client → `gsp`'s public listener → WireGuard
   tunnel (boringtun userspace backend) → `gsp-agent`'s interface → the
   echo backend → back, confirmed stable (a live handshake surviving many
   re-registration cycles, not just a one-shot connect) — see `HANDOVER.md`
   for the two real bugs this surfaced and fixed: `gsp-agent` never added
   the edge proxy as a peer at all (closed with `--peer-pubkey`/
   `--peer-endpoint`), and `tunnel_client::run` tore down and rebuilt the
   WireGuard session on every re-registration even when nothing changed
   (boringtun panics on a same-pubkey `configure_peer`, and the constant
   churn never let a handshake stabilize) — fixed by skipping a
   registration identical to the last one applied.

7. ✅ **Proxy-peers registry — the mirror-image direction**: slice 6's
   end-to-end verification shipped `gsp-agent --peer-pubkey`/
   `--peer-endpoint` as a static single-proxy pin, which proved the tunnel
   data plane but doesn't scale — a growing proxy fleet, or a proxy added
   after an origin was already deployed, needed the origin's `gsp-agent`
   restarted with new flags to learn about it. Closed by mirroring slices
   2+4's mechanism in the other direction: a new `gsp-controller`
   "proxy peers" registry (`gsp_controller::proxy_peers`, `POST`/`GET
   /proxy-peers(+/{name})`, `GET /proxy-peers/subscribe`) that every
   `gsp --tunnel-*` instance registers itself into (new `gsp::
   proxy_register`, mirrors `gsp-agent::register`), and a new `gsp-agent`
   subscribe-and-reconcile task (`proxy_subscribe.rs`, mirrors `gsp`'s own
   `tunnel_client.rs`, including the same remove-then-add + skip-unchanged
   fix slice 6 needed) that reconciles every registered proxy onto the
   origin's interface. `--peer-pubkey`/`--peer-endpoint` still work
   alongside it as a manual pin — the two converge to the same state, no
   conflict. Live-verified (2026-10-01) by `make tunnel-e2e` scenario 3: a
   proxy started after the origin is learned by the already-running agent
   with no restart, and carries traffic. **That run also found a real bug**:
   every proxy peer got `AllowedIPs 0.0.0.0/0`, so a *second* proxy stole the
   first one's route. Fixed 2026-10-02 with the tunnel address authority
   (`docs/11` "Address authority"): peers are `/32`s, and
   `two_proxies_share_one_origin` now passes in the lab.

Still open (see docs/11 "Open questions"): the CGNAT/both-sides-restrictive-
NAT fallback (documented v1 limitation, no code) — does not block starting
slice 1.

## Later / optional
- QUIC-CID-aware sniffer & session keying.
- Cross-instance session handover (shared *session* state) — still out of scope;
  the phase 10–13 control plane shares config and health, never sessions.
- eBPF/XDP pre-filter to drop floods before user space.
- Optional TLS/DTLS wrapping (proxy terminates, backend plain).
- **`gsp-ui` leftovers** — phase 10+11 slice 11f shipped a functional PoC; the
  visual redesign (commit `d86c786`, `docs/10` "The admin GUI" → Redesign) and
  then confirmation dialogs for destructive actions, in-flight button states, a
  vitest frontend suite and a table-driven header-forwarding contract test
  (`crates/gsp-ui/src/proxy_util.rs`) landed. Still open: a confirmation step
  on config *submit*, and browser-level end-to-end tests.
- **`deploy/`** — reference Dockerfile (five targets on distroless), compose
  control-plane demo and plain k8s manifests, smoke-tested by the CI `deploy` job
  (`docs/12`; blocking since 2026-10-02).
- **Phase 14 follow-ups** — the multi-proxy `AllowedIPs` fix and the controller-allocated
  tunnel address authority landed 2026-10-02 (`docs/11` "Address authority"); the
  CI-friendly tunnel e2e exists (`make tunnel-e2e`, CI job `tunnel`). Remaining pieces
  (IPv6, lease expiry, HA-replicated allocation, a UI view) are in HANDOVER "Known follow-ups".

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
- **v2.1**: + phase 14 (WireGuard backend transport — `gsp-agent`, backend /
  proxy-peers registries; proxies reach origins on a different network) — see
  [11-backend-transport.md](11-backend-transport.md).
