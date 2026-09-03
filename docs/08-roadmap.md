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

## Phase 4 – External routing logic (week 11–12)
- Resolver client (gRPC + HTTP), request/response schema.
- Result cache with positive/negative TTL, configurable key.
- `on_error` strategies (reject/fallback/stale_ok).
- **Result**: matchmaker integration, token→instance routing.

## Phase 5 – Operations & zero-downtime (week 13–14)
- Hot reload (SIGHUP + file watch), atomic snapshot swap.
- Admin API: backends CRUD, `draining`, read snapshot, `drain`/`readyz`.
- Graceful draining (backend & proxy instance), `SIGTERM` flow.
- Passive health signals from the data path.
- **Result**: a production-ready deploy/update cycle.

## Phase 6 – Client-IP preservation (week 15–16)
- PROXY protocol v1/v2 (TCP), v2-UDP variant.
- Transparent mode (TPROXY) including docs for the network/routing setup.
- **Result**: backends see the real client IP.

## Phase 7 – Security & hardening (week 17–18)
- Filter chain: CIDR allow/deny (LPM trie), rate limit (src_ip + /24), global caps.
- UDP first-packet gate, automated tests for the amplifier checklist.
- Optional geo filter.
- Fuzzing of the peek/sniffer parsers, load tests against the NFRs.
- **Result**: hardened against common L4/7 abuse.

## Phase 8 – Discovery & scaling (week 19–20)
- `BackendSource` adapters: DNS SRV, Kubernetes Endpoints, Consul.
- HA docs: anycast/L4 LB in front, capacity planning, dashboards & alerts.
- **Result**: dynamic backend fleets, horizontal scaling.

## Phase 9 – Sniffer plugin loader
- A separate community repository of game-protocol sniffers, loaded into the
  proxy at runtime — not compiled in, not a fork.
- Sandboxed execution (likely **WASM** via wasmtime/extism: no host syscalls
  except a granted `sniff(&[u8]) -> RouteHint` ABI); bounded time / memory per
  call; input capped at `peek_max_bytes`.
- Config: a plugins directory + per-listener `sniffer:` name resolved against
  the loaded set; reload picks up added/removed modules.
- A generic `first-bytes` `regex` matcher would ship as one of these plugins
  (precompiled, bounded `N`) rather than pulling `regex` onto the core routing
  path.
- Supply chain: module signing / pinning; the proxy ships a small first-party
  set (e.g. `minecraft`, `a2s`, `sni`) built the same way, no special-casing.
- Open question: latency of the WASM boundary vs. the NFR budget — measure
  before committing; the out-of-process external resolver (Phase 4) is the
  fallback if the in-process boundary is too costly.

## Later / optional
- QUIC-CID-aware sniffer & session keying.
- Cross-instance session handover (shared state).
- eBPF/XDP pre-filter to drop floods before user space.
- Optional TLS/DTLS wrapping (proxy terminates, backend plain).
- Web UI for the admin API.

## Milestone cuts
- **MVP**: phase 0–2 (L4 TCP+UDP, static, health, metrics).
- **v1.0**: + phase 3–5 (routing, resolver, zero-downtime).
- **v1.1**: + phase 6–7 (client IP, hardening).
- **v1.2**: + phase 8 (discovery, HA operations docs).
