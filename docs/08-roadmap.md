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
- ✅ Bidirectional buffered pump; per-direction idle timeout, connect timeout,
  half-close. (`splice()` fast path deferred — slots in behind the same function.)
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
  (lock-free) session table. (`recvmmsg`/`sendmmsg` batching and a timing wheel
  deferred — plain `recv_from`/`send` and a 1 s idle sweep for now.)
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
  LRU cache. (`backend_sources:` live reload is still restart-only — its own
  slice; needs per-source refresh-task management like `ListenerManager`.)
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
- Deferred: k8s watch informer; per-`backend_sources` live reload (startup-only
  today, like `workers`).

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

### Slices
- ✅ **Slice 1**: registry threading (pure refactor). `sniff::sniffer` global fn →
  `gsp_core::sniff::Sniffers` (a `HashMap<String, Arc<dyn Sniffer>>` registry,
  `register`/`get`), held as `Arc<Sniffers>` on `Runtime` /
  `ListenerManager` and threaded to every listener worker exactly like
  `Arc<Resolvers>` / `Option<Arc<GeoDb>>` (`Runtime::start_with_sniffers`,
  `ListenerManager::new` gained the param, `run_tcp_listener` /
  `run_udp_listener` / `open_session` too). `warn_if_missing` now takes the
  registry. No built-in sniffers register in production
  (`Sniffers::default()` is empty); tests build a registry with `test-host`.
  No behaviour change, no `wasmtime`.
- ✅ **Slice 2**: `settings.sniffers` schema — `dir`, `call_timeout_ms`
  (default 20), `max_memory_bytes` (default 16 MiB), `modules: [{ name,
  sha256 }]` → `gsp_config::SniffersConfig` on `Config::sniffers` (`None` when
  the block is absent). `validate()` rejects an empty `dir`, a zero timeout /
  memory cap, and a bad `sha256` (must be 64 hex chars — case-normalised to
  lowercase). `config.example.yaml` + `docs/05` (schema block, a validation
  bullet, and a reload-semantics row — restart-only until slice 4's rescan).
  Config-only: nothing reads `Config::sniffers` yet (that's slice 3).
- ✅ **Slice 3**: `WasmSniffer` in `gsp` (`crates/gsp/src/sniffer_loader.rs`) —
  a shared `wasmtime::Engine` (epoch interruption on) + one epoch-ticker thread
  per `build_sniffers` call (`engine.increment_epoch()` every
  `call_timeout_ms`); a fresh `Store<StoreState>` per call with a `StoreLimits`
  memory cap (`max_memory_bytes`) and a one-tick epoch deadline. ABI: guest
  exports `memory`, `alloc(len) -> ptr`, `sniff(ptr,len) -> packed(ptr,len)|0`
  (widened to `sniff(in_ptr, in_len, cfg_ptr, cfg_len)` by ADR 16a — per-plugin
  config); host writes the input via `alloc`+`memory.write`, calls `sniff`, and decodes
  a compact `RouteHint` encoding (flags byte + length-prefixed UTF-8 strings)
  from the returned region — any out-of-bounds pointer/length or malformed
  encoding is `bad_output`, never a panic. `Sniffer::name` changed
  `&'static str` → `&str` (a WASM plugin's name comes from its file stem, not
  a compiled-in constant). `build_sniffers(&SniffersConfig)` scans `dir` for
  `*.wasm`, verifies each against `modules[].sha256` when pins are configured
  (`sha2` dep), and returns a `gsp_core::sniff::Sniffers` registry; `main.rs`
  calls it for both `--check` and startup (fails closed on a bad plugin dir,
  same as `geo_db`) and passes the result to `Runtime::start_with_discovery`.
  Metrics `gsp_sniffer_calls_total{name,result}` /
  `gsp_sniffer_call_seconds{name}` (`metrics_defs.rs` + `docs/06`). Tests
  (`crates/gsp/src/sniffer_loader.rs`, 6): the `RouteHint` decoder's happy path
  + truncated/bad-UTF-8 rejection, an end-to-end call against a hand-written
  WAT fixture (no `wasm32-unknown-unknown` toolchain needed — the `wat` crate
  parses WAT text to bytes at test time, dev-dependency only), an
  infinite-loop plugin proving the epoch deadline actually traps instead of
  hanging the call, `build_sniffers` loading from a scratch dir, and
  `sha256` pin enforcement (wrong pin rejected, right pin loads). Latency-ledger
  entry still owed to slice 6's bench (per-call instantiate cost vs. NFR N1) —
  today's cost is: one `Store`/`Instance` per call, only on listeners with a
  `sniffer:` route (already gated by `peek_len`), never on the accept loop.
- ✅ **Slice 4**: reload rescans `dir` — added modules load, removed drop,
  changed (hash) recompile; the registry is swapped like the snapshot.
  `gsp_core::sniff::Sniffers` grew interior mutability (an `ArcSwap` over its
  name→plugin map, mirroring `RouteHints`): every `Arc<Sniffers>` clone handed
  to a listener worker at spawn time points at the *same* instance, so
  `Sniffers::replace(map)` from the reload task is visible everywhere
  instantly — no replumbing through `Runtime`/`ListenerManager` needed, unlike
  the snapshot swap. `Sniffer::get` now returns an owned `Arc<dyn Sniffer>`
  (was `&Arc<dyn Sniffer>`) so callers aren't holding a reference into a table
  that can be swapped out from under them. The `gsp` binary's
  `sniffer_loader::SnifferLoader` holds only the shared `wasmtime::Engine` /
  epoch-ticker thread (built once at startup) and exposes `scan(&SniffersConfig)
  -> HashMap<name, Arc<dyn Sniffer>>`; `build_sniffers` (startup) is now
  `SnifferLoader::new` + one `scan`. `main.rs` keeps the loader alive and
  passes it (plus the live `Arc<Sniffers>`) into `reload::run`, which rescans
  on every applied reload — a scan error keeps the previous plugin set (never
  half-applies); `settings.sniffers` appearing/disappearing between reloads is
  logged as needing a restart, matching `docs/05`. Engine params
  (`call_timeout_ms`/`max_memory_bytes`) are still startup-only.
- ✅ **Slice 5**: first-party plugin crates, a standalone workspace
  `crates/plugins/` (own `[workspace]`, like `crates/gsp-config/fuzz` — never
  a dependency of `gsp`/`gsp-core`, so `make check` needs no wasm target).
  `gsp-sniffer-abi`: the guest-side write half of the ABI slice 3 defined —
  `alloc(len)` delegating to the module's own global allocator (not a
  hand-rolled bump pointer, so it never collides with the plugin's own
  allocations), `encode(&Hint) -> Vec<u8>` (pure, unit-testable on any host)
  and `emit_hint` (places `encode`'s bytes via `alloc`, packs the
  `(ptr<<32)|len` result). Three plugins, each `crate-type = ["cdylib",
  "lib"]` so `recognise()` is a plain testable native fn and `sniff` is a
  thin `#[no_mangle] extern "C"` wrapper: `a2s` (Source-engine query packets,
  `0xFFFFFFFF` + a query-type byte, tags `key: "a2s"` — no hostname to
  extract); `minecraft` (parses the protocol ≥ 1.7 Handshake's `server
  address` field — the same virtual-host trick BungeeCord/Velocity use —
  strips a Forge `\0FML\0` suffix, lower-cases); `regex-firstbytes` (a
  **template**, not a generic engine — the config schema has no per-plugin
  parameters yet, so a runtime pattern can't be handed in; hard-codes an
  HTTP/1.x request-line matcher to demonstrate the bounded shape the roadmap
  describes). `make plugins` (native `cargo test --workspace` +
  `--release --target wasm32-unknown-unknown` build, ~17–21 KiB per module
  with `opt-level=z, lto, panic=abort, strip`); new CI job `plugins`
  (installs the wasm32 target, native tests, wasm build, size print). 13 new
  native unit tests across the four crates, plus one `#[ignore]`d integration
  test in `gsp`'s own `sniffer_loader.rs` that loads the *actual built*
  `.wasm` files and drives each through the real `wasmtime` loader end to end
  (`cargo test -p gsp plugin_artifacts -- --ignored`, after `make plugins`).
  `crates/plugins/README.md` has the build/install walkthrough. Known gap
  (noted there and in `HANDOVER.md`): no per-plugin config path exists yet,
  so `regex-firstbytes` can't be a true generic engine — a future config
  extension (e.g. `modules[].config`) would be needed.
- ✅ **Slice 6**: WASM-boundary latency bench vs. N1 — implemented as
  `sniffer_loader::tests::wasm_boundary_latency_vs_nfr_n1` in
  `crates/gsp/src/sniffer_loader.rs` (an `#[ignore]`d test, same convention
  as slice 5's artifact test; `cargo test -p gsp --release wasm_boundary --
  --ignored --nocapture` after `make plugins`), rather than extending
  `gsp-bench` (that crate only depends on `gsp-core`, and the loader is
  `gsp`-binary-only — reusing the existing test harness avoided a new
  cross-crate seam for a one-off measurement) or adding `criterion` (kept the
  project's existing non-criterion, custom-harness style, matching
  `gsp-bench`'s own `Stats`/percentile approach). Times the real, compiled
  first-party plugins (not synthetic WAT fixtures) through the actual
  `alloc`/`memory.write`/`sniff`/decode round trip. **Result: all three pass
  N1 with wide margin** — p50 8–10 µs, p99 12–26 µs (loopback, this box; see
  `docs/07` for the full table) — so the fresh-`Store`-per-call design from
  slice 3 needed none of the `InstancePre` / warm-instance fallbacks the
  locked decision held in reserve. Building this bench surfaced a real
  interaction worth documenting: the epoch ticker (slice 3) fires on
  wall-clock time shared across a loader's whole lifetime, so a call that
  happens to straddle a tick boundary legitimately traps even at ~10 µs of
  actual work — the bench uses a long `call_timeout` to get a clean
  measurement, and `docs/07`'s new "plugin sandbox guarantees" section notes
  that `call_timeout_ms` is therefore a *ceiling*, not a per-call guarantee
  of the full budget. Module `sha256` pin verification was already in slice
  2/3 (`settings.sniffers.modules`); `docs/07` "plugin sandbox guarantees"
  section covers the full no-WASI/no-host-imports contract, the two runtime
  bounds, and confirms a sniffer's lack of a reply path leaves the amplifier
  checklist untouched.
- ✅ **Slice 7 — phase 9 complete**: end-to-end test — a trivial
  `host-echo.wasm` fixture (compiled inline from WAT via the `wat` crate, no
  `wasm32-unknown-unknown` toolchain needed — the same `HOST_SNIFFER_WAT`
  fixture slice 3's own tests already used), loaded through the *real*
  `build_sniffers` directory scan (not a hand-constructed `WasmSniffer`),
  driving a live `Runtime` and a real TCP connection routed by the plugin's
  hint host
  (`sniffer_loader::tests::end_to_end_connection_routes_by_a_real_wasm_plugins_hint`
  — mirrors `sniff::tests::sniffer_matcher_routes_a_connection_by_hint_host`,
  slice 1's native-sniffer version, but through the whole WASM path instead).
  Not `#[ignore]`d — it needs neither `make plugins` nor the wasm target,
  so it runs in the normal `cargo test -p gsp` / `make check` pass, unlike
  slices 5–6's artifact/latency tests (which need the real compiled
  first-party plugins). Those two *are* now wired into CI: the `plugins` job
  installs `protoc` too and, after building the wasm modules, runs
  `cargo test -p gsp --release -- --ignored --nocapture` — so the artifact
  round-trip and the N1 latency gate are both enforced on every push/PR, not
  just documented as reproducible locally.

### Risks
- `wasmtime` is a large dependency and adds build time; CI needs
  `rustup target add wasm32-unknown-unknown` and an engine cache — done
  (`plugins` CI job, `Swatinem/rust-cache` on both workspaces).
- The epoch-ticker is a real background thread — recorded in `HANDOVER.md`
  (slice 3) and `docs/07`'s sandbox-guarantees section (slice 6), including
  the "it's a ceiling, not a guarantee" caveat slice 6 uncovered. Not yet
  added to `docs/02`'s threading model table — worth a follow-up doc pass.
- Warm-instance reuse: **not needed** — slice 6 measured the fresh-per-call
  design comfortably inside NFR N1 (p50 8–10 µs, real first-party plugins),
  so this risk didn't materialise. Left documented in case a future,
  heavier plugin changes that calculus.

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

## Phase 10 – Fleet aggregation & operational Web UI
Full design: [10-distributed-control-plane.md](10-distributed-control-plane.md)
("The admin GUI", level 1). **No new source of truth**, nothing on the data
path.
- A stateless aggregator (a mode of `gsp-controller`, or a small `gsp-aggregator`):
  fan `GET /config` / `/pools` / `/metrics` / `/healthz` out to a configured or
  discovered instance list and merge — the single fleet view.
- Fan-out for the phase-5 intent verbs (drain / add / remove a backend,
  `route-hint`, drain an instance) to every instance at once.
- Web UI over that: fleet dashboard, pool / backend table, per-instance health,
  the operational actions. Read-heavy; **no structural config editing**.
- Auth (bearer / OIDC) on the aggregator + UI; each proxy's admin API is locked
  to the aggregator's identity / network.
- Not solved here: intent still evaporates on instance restart, and is not
  guaranteed consistent across the fleet (Phase 11).
- **Result**: one screen to watch and operate the whole fleet.

## Phase 11 – Global config & intent store + controller
Full design: [10-distributed-control-plane.md](10-distributed-control-plane.md)
(Tier 1 + "The controller"). First release with **cross-instance shared config
and intent** — supersedes ADR 4 for config/intent (never for sessions).
- `gsp-controller`: owns the Tier-1 ordered / versioned / durable store (backing
  store per a new ADR — embed / etcd / git).
- `gsp` grows `config_source: file | store | file+store` and a store
  subscription client (in the binary, not `gsp-core` — same seam as resolvers):
  subscribe → full snapshot + cursor → change stream → feed the **existing**
  `validate() → Snapshot::build → ArcSwap::store` path. Invalid revision ⇒
  reject + keep previous, like a bad file reload.
- Operator intent (backend overlay, admin state, route hints, resolver pins)
  moves into the store; a restarted instance recovers it. The phase-5 admin
  verbs become "controller writes a revision"; direct per-instance admin stays
  as break-glass.
- Controller config API: `validate()`, revisions, history, diff, one-key
  rollback, staged / canary rollout.
- Web UI gains structural editing + revision history + RBAC.
- Controller HA: N replicas, leader lock for writes; an outage freezes changes,
  not traffic.
- **Result**: manage the whole fleet's configuration from one place,
  persistently, with an audit trail.

## Phase 12 – Regional health fabric
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
  the Phase 11–12 control plane shares config and health, never sessions.
- eBPF/XDP pre-filter to drop floods before user space.
- Optional TLS/DTLS wrapping (proxy terminates, backend plain).

## Milestone cuts
- **MVP**: phase 0–2 (L4 TCP+UDP, static, health, metrics).
- **v1.0**: + phase 3–5 (routing, resolver, zero-downtime).
- **v1.1**: + phase 6–7 (client IP, hardening).
- **v1.2**: + phase 8 (discovery, HA operations docs).
- **v1.3**: + phase 9–10 (sniffer plugin loader; fleet view & operational Web
  UI). Both additive — the data-plane contract is unchanged.
- **v2.0**: + phase 11–12 (global config / intent control plane; regional health
  fabric). First release with cross-instance shared state, for config and health
  only — see [10-distributed-control-plane.md](10-distributed-control-plane.md).
