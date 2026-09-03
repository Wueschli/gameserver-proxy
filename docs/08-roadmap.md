# 08 – Roadmap

Incremental. Each phase is usable on its own.

## Phase 0 – Skeleton (week 1–2)
- Project setup, CI, lint, test harness.
- Config loading + validation + snapshot data structure (no reload yet).
- Structured logging, `/healthz`, `/metrics` skeleton, `build_info`.
- **Result**: the process starts with a config and exposes basic metrics.

## Phase 1 – L4 TCP proxy (week 3–4)
- TCP listener with `SO_REUSEPORT`, one worker per core.
- Static listener→pool mapping, `round_robin` + `least_conn`.
- Bidirectional pump with `splice()` + fallback.
- Connect/idle timeouts, half-close.
- Active `tcp_connect` health checks; backend states.
- Core metrics (connections, bytes, duration, backend errors).
- **Result**: usable as a plain TCP game server front proxy.

## Phase 2 – L4 UDP proxy (week 5–7)
- UDP listener, `recvmmsg`/`sendmmsg`, thread-local session table, timing wheel.
- Per-session `connect(2)` upstream socket.
- Session affinity (`hash_on: src_ip` + sticky table), `consistent_hash`.
- `udp_probe` health check.
- Session caps, per-route idle timeout.
- **Result**: covers the majority of real-time game servers.

## Phase 3 – Routing intelligence (week 8–10)
- Route rule list with priorities.
- Matchers: `always`, `port`, `client-cidr`, `first-bytes` (prefix/regex/length).
- `dst` matcher + prefix listener (one socket, `IP_PKTINFO`/`getsockname`) for games
  with no protocol hint (subdomain → its own destination IP). Optional push resolver
  (`/route-hint`).
- SNI peek matcher (without TLS termination).
- Sniffer plugin API (in-process, static): `sni`, `minecraft`, `a2s` as references.
- **Result**: multiple games/regions behind one port.

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

## Later / optional
- QUIC-CID-aware sniffer & session keying.
- WASM / out-of-process plugins.
- Cross-instance session handover (shared state).
- eBPF/XDP pre-filter to drop floods before user space.
- Optional TLS/DTLS wrapping (proxy terminates, backend plain).
- Web UI for the admin API.

## Milestone cuts
- **MVP**: phase 0–2 (L4 TCP+UDP, static, health, metrics).
- **v1.0**: + phase 3–5 (routing, resolver, zero-downtime).
- **v1.1**: + phase 6–7 (client IP, hardening).
- **v1.2**: + phase 8 (discovery, HA operations docs).
