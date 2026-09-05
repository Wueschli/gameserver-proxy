# 06 – Operations & observability

## Metrics (Prometheus)

### Listener / connections
- `gsp_listener_connections_total{listener,result}` – `result` =
  `accepted|no_route|sniffer_reject` (`sniffer_reject`: a `sniffer` plugin
  returned a `reject` hint for the first bytes; the connection is dropped
  before routing). ACL / rate-limit / geo / cap rejections do **not** get a
  `result` value here — they increment `gsp_filter_blocked_total` instead
  (see "Security / filter chain" below), and a resolver miss/error is folded
  into `no_route` rather than its own value. No `protocol` label — split by
  `listener` name instead (a listener has one fixed protocol).
- `gsp_active_connections{listener}` (gauge) — no `protocol` label.
- `gsp_active_udp_sessions{listener}` (gauge)
- `gsp_connection_duration_seconds{listener}` (histogram) — no `pool` label
  (a connection's pool can change across a reload; the listener is stable).
- `gsp_session_setup_seconds{listener,phase}` — **planned, not implemented**;
  see "Planned / not yet built" below.

### Throughput
- `gsp_bytes_total{listener,dir}` – `dir` = `c2s|s2c`. No `pool` label.
- `gsp_packets_total{listener,dir}` (UDP). No `pool` label.
- `gsp_datagrams_dropped_total{listener,reason}` — v0 `reason` =
  `no_route|no_backend|upstream_bind|upstream_send|outside_prefix|draining|reply_bind|first_packet_gate|sniffer_reject`
  (`outside_prefix`: prefix-mode listener, datagram destination not in `prefix`;
  `first_packet_gate`: `first_packet_gate` listener, first datagram not recognised;
  `sniffer_reject`: a `sniffer` plugin returned a `reject` hint — no session, no reply)

### Upstream / pool
- `gsp_pool_backends{pool,state}` (gauge; `state` = healthy|unhealthy|draining|disabled)
- `gsp_backend_active_sessions{pool,backend}` (gauge)
- `gsp_backend_connect_errors_total{pool,backend,kind}` – `kind` = `timeout|refused|unreachable`
- `gsp_healthcheck_total{pool,backend,result}`
- `gsp_lb_selections_total{pool,strategy,result}` – `strategy` =
  `round_robin|least_conn|consistent_hash|weighted`; `result` = `ok|no_backend|at_capacity`
- `gsp_route_hints_applied_total{listener}` – a `POST /route-hint` entry decided
  routing for a connection / UDP session
- `gsp_proxy_protocol_headers_total{pool,version}` – `version` = `v1|v2|v2-udp`; a
  PROXY protocol header was prepended to an upstream connection (TCP) or the first
  datagram of a session (UDP `v2-udp`). `pool="(resolver target)"` for a pool-less
  resolver `target` connection (the header form comes from the resolver's
  `proxy_protocol:`)

### Backend discovery (phase 8)
- `gsp_discovery_refresh_total{pool,kind,result}` – `kind` =
  `dns_srv|consul|kubernetes`; `result` = `ok|empty|error`. One per refresh
  attempt. `empty` / `error` keep the last-known-good backend set (the pool is
  never cleared by a failed refresh).
- `gsp_discovery_backends{pool}` (gauge) – addresses returned by the pool's
  source at its last successful refresh.

### Security / filter chain
- `gsp_filter_blocked_total{listener,filter}` – `filter` = `acl` | `geo` |
  `rate_ip` | `rate_net` | `src_conn_ip` | `src_conn_net` | `max_conn` |
  `max_udp` | `max_new_rate`; a connection / new UDP session was dropped by the
  pre-routing filter chain — `acl` = source-IP allow/deny, `geo` = GeoIP country
  allow/deny (or the DB failed to load and the listener fails closed), `rate_ip`
  / `rate_net` = the per-listener per-IP / per-/24 (per-/64) token bucket was
  empty, `src_conn_ip` / `src_conn_net` = the per-listener concurrent per-source
  connection / session cap was reached, `max_conn` / `max_udp` / `max_new_rate` =
  a process-wide `settings.limits` cap was hit

### Sniffer plugins (phase 9)
- `gsp_sniffer_calls_total{name,result}` – `result` = `ok` | `unrecognised` |
  `timeout` | `trap` | `bad_output`; one increment per `sniff()` call through
  the WASM loader. `timeout` = the epoch-interruption deadline
  (`settings.sniffers.call_timeout_ms`) fired; `trap` = any other WASM trap or
  an instantiation failure; `bad_output` = the plugin returned a result the
  host couldn't decode as a `RouteHint`.
- `gsp_sniffer_call_seconds{name}` (histogram) – wall-clock time of one
  `sniff()` call, instantiation included.

### Resolver
- `gsp_resolver_requests_total{resolver,result}` – `ok|empty|timeout|error`
- `gsp_resolver_cache_total{resolver,result}` – `hit|hit_negative|miss|stale|uncacheable`

### Proxy internals
- `gsp_config_reload_total{result}` / `gsp_config_version` (gauge, timestamp)
- `gsp_build_info{version,commit}` (gauge, always `1`) — set once at startup;
  `commit` is a 12-char git SHA baked in at build time (`crates/gsp/build.rs`,
  `"unknown"` if `.git` isn't available, e.g. a source tarball).
- `gsp_fd_open` (gauge, no labels) — this process's open file descriptor count
  (`/proc/self/fd` on Linux; absent elsewhere), sampled every 5 s by a small
  background task (`crates/gsp/src/procinfo.rs`), independent of the
  health-check sweep.
- `gsp_fd_limit` (gauge, no labels) — this process's `RLIMIT_NOFILE` soft
  limit (`getrlimit`, via `nix`), sampled once at startup (it doesn't change
  at runtime).

**Added RTT**: no built-in RTT SLO metric exists yet — `make bench`'s added
p50/p99 (vs. NFR N1/N2) is the closest thing today, measured out-of-band, not
exported as a `/metrics` series. See "Planned / not yet built" below.

### Planned / not yet built

Documented here as real future work, not implemented — none of these exist in
`metrics_defs.rs` today, so don't expect them on `/metrics` yet:

- **`gsp_resolver_latency_seconds{resolver}`**, **`gsp_session_setup_seconds{listener,phase}`**,
  **`gsp_worker_busy_ratio{worker}`** — plausible finer-grained latency /
  saturation instrumentation, deferred until a real debugging need shows the
  existing aggregate metrics (`gsp_resolver_requests_total`,
  `gsp-bench`'s added-latency numbers) aren't enough to explain a slowdown.
  Not worth the hot-path cost speculatively.
- ~~`gsp_resolver_cache{resolver,state}`~~ / ~~`gsp_buffer_pool_exhausted_total`~~
  — dropped from the plan: the former was a duplicate of
  `gsp_resolver_cache_total` above (typo'd as a second metric), the latter
  described a buffer-pool architecture that was never built (per-worker fixed
  buffers replaced it early on — see `docs/09` "Key libraries").

## Connection logs (structured, JSON)

One event on session **close** (plus optionally on start), sampleable:

```json
{
  "ts": "2026-09-03T10:48:12Z",
  "event": "session_close",
  "listener": "public-udp",
  "protocol": "udp",
  "src": "203.0.113.7:51343",
  "route": "route#1",
  "resolver": "matchmaker",
  "pool": "match-eu",
  "backend": "10.1.0.12:7777",
  "sticky_key": "player:42",
  "bytes_c2s": 184320,
  "bytes_s2c": 942080,
  "pkts_c2s": 1440,
  "pkts_s2c": 1460,
  "duration_ms": 812345,
  "setup_ms": 6,
  "close_reason": "idle_timeout",
  "proxy_protocol": "none"
}
```

`close_reason`: `client_close|backend_close|idle_timeout|drain|limit|backend_down|error`.

## Tracing (optional, OpenTelemetry)

Trace only the **setup phase** (not the byte stream): span `session.setup` with child
spans `peek`, `route.match`, `resolver.call`, `backend.connect`. The trace ID can be
carried in the PROXY v2 TLV if the backend should correlate it.

## The proxy's own health endpoints

- `GET /healthz` – the process is alive.
- `GET /readyz` – config loaded & valid, at least one listener bound, and not
  drained. Returns `draining` (503) after `POST /admin/drain`.
- `GET /metrics` – Prometheus.
- `GET /config` – plaintext view of the active snapshot (listeners + pools +
  `draining` / `active_conns`). Implemented (phase 5). `GET /pools` – per-pool
  backend list with health / `state=` / active count.
- `GET /sessions[?listener=&pool=&proto=tcp|udp&src=<ip>]` – plaintext list of
  every live proxied connection / UDP session: id, transport, listener, client
  (`peer=`) and local (`local=`) address, chosen `pool=` / `backend=`, and
  `age=`. Backed by a small `id → metadata` registry on the connection tracker
  (one brief lock per connection at open / close, nothing on the per-byte path).
  Point-in-time; the optional query params filter by exact `listener` / `pool`,
  transport, or source IP. Implemented.
- `POST /admin/drain` / `POST /admin/undrain` – take this instance out of / back
  into LB rotation by flipping `readyz`, without stopping the data path.
  Implemented (phase 5).
- `POST /route-hint` `{src_ip, pool, ttl_sec}` – push-resolver hint (scheme C);
  applied by listeners with `route_hint: true`. Implemented.
- `PATCH /pools/{pool}/backends/{addr}` `{state: enabled|draining|disabled}` –
  set an operator backend state. Implemented (phase 5). `draining` / `disabled`
  remove the backend from new-session selection (including UDP affinity)
  immediately; existing sessions keep running. Carried across a config reload by
  address. `GET /pools` shows the current `state=` per backend.

## Backend health checks

- **Active**: `tcp_connect` / `udp_probe` (send/expect bytes). Parameters:
  `interval`, `timeout`, `rise`, `fall`. Checks run in the control plane, the
  result → a new snapshot.
  An `http` kind (ping a sidecar health port) is **planned, not implemented** —
  `HealthCheckKind` only has `TcpConnect`/`UdpProbe` today; deferred until a
  real backend that exposes an HTTP health endpoint needs it.
- **Passive**: the data path reports `connect refused/timeout`, early RST, ICMP
  unreachable. After `n` errors within `t` seconds → backend `unhealthy` (faster than
  the active check).
- **Recovery**: back to `healthy` only after `rise` successful active checks.

## Graceful draining & deployments

Flow for swapping a backend without dropping players:

1. Register the new backend (`POST /pools/{p}/backends`) → `healthy` after `rise`
   checks.
2. Set the old backend `PATCH state=draining` → the LB assigns no new sessions there;
   sticky keys pointing to it are re-resolved on next contact.
3. Wait until `gsp_backend_active_sessions` = 0 **or** `drain_deadline` is reached.
4. After the deadline: end remaining sessions with `close_reason=drain`.
5. Remove the old backend.

For a **proxy instance** restart:
1. Set `readyz` to "false" (`POST /admin/drain`; `POST /admin/undrain` reverts) →
   the upstream LB/anycast takes the instance out of rotation. The data path keeps
   running; watch `active_conns` (in `GET /config`) fall.
2. Grace period: no new `accept`s, existing sessions keep running.
3. After `settings.shutdown_grace_sec` (default 30) close remaining sessions, exit
   the process.
4. `SIGINT` / `SIGTERM` triggers exactly this flow (implemented, phase 5): the TCP
   accept loops and the UDP recv loops stop taking new work immediately; in-flight
   TCP connections and established UDP sessions keep running and are waited on (UDP
   sessions until they idle out); once all have finished — or the grace period
   expires — the process exits. New datagrams to a draining UDP listener are
   dropped (`gsp_datagrams_dropped_total{reason="draining"}`).

## Multi-instance operations (HA)

Production is many identical, **independent** `gsp` instances across hosts / AZs /
regions, fronted by **anycast or an L4 load balancer**. No shared data-plane
state (ADR 4): lose an instance and the LB / anycast spreads its clients onto the
rest; affected sessions reconnect.

- **Config distribution**: either the file-based approach below, or the phase
  10+11 controller (see "Fleet control plane" below) — bake into the image
  (change = rolling redeploy), render the file from a store (ConfigMap /
  `consul-template` / Ansible) and let file-watch pick it up in seconds, or
  point every instance at a `gsp-controller` with `--controller <url>`. Either
  way the structural config is hot-reloaded; validate-before-swap keeps a bad
  config from taking an instance down (the controller rejects it at
  submission time instead, one hop earlier — see below).
- **Backend membership** comes from phase-8 discovery (`backend_sources`: DNS SRV
  / Consul / k8s Endpoints), not per-instance file edits. Every instance runs the
  same source config and converges independently; a source outage freezes the
  set at last-known-good rather than draining a pool fleet-wide.
- **Operator intent** (drain / add / remove a backend, route hints) is still
  applied per instance, either directly against that instance's admin API or
  fanned out through `gsp-aggregator` (see below), and is **not persisted or
  replayed after a restart** — moving intent into the controller's revision
  log is phase 12, not this release.
- **`transparent: true` needs the backend's return path through the same instance**
  that owns the connection (per-flow-consistent ECMP / anycast). PROXY protocol
  has no such constraint — prefer it when the fronting layer can rehash.
- Aggregate `/metrics` and `/pools` yourself in Prometheus for per-instance detail;
  for a single fleet-wide read/operate surface, see `gsp-aggregator` and
  `gsp-ui` below — there is still no fleet-wide metrics aggregation.

## Fleet control plane (phase 10+11)

Three additional, independent binaries — a single-tier PoC (`docs/10`'s
`standalone`/`slave` hierarchy, intra-tier HA, and moving intent into the
controller's revision log are phase 12, design-only). None of them expose
`GET /metrics`; the Prometheus surface stays per-`gsp`-instance as above. All
three serve unauthenticated `GET /healthz` for liveness regardless of their
auth settings below.

### `gsp-controller` — structural config distribution

One `--auth-token`-gated (optional) surface, backed by an embedded `sled`
store that never forgets a revision (ADR 20):

- `POST /config` — submit a raw YAML config (the same document a `gsp
  --config` file would hold). Validated with the same `gsp_config::parse_str`
  a file reload runs; a rejected submission (`422`, JSON error body) leaves
  the current revision untouched — the revision only advances on success.
  Response body is `{"revision": N}` (the new revision only appears here, not
  as a header).
- `GET /config` — the current revision's raw text, with `X-Config-Revision`
  header. `404` before any submission.
- `GET /config/subscribe?since=<revision>` (SSE) — catch-up range then a live
  tail; `gsp --controller <url>` consumes this to hot-reload. A lagging
  subscriber just re-runs the catch-up query against the store — no delivery
  state kept on the writer side.
- `GET /config/revisions` — history (revision, size, whether it's `current`).
- `GET /config/revisions/{revision}` — a past revision's raw text.
- `GET /config/revisions/{revision}/diff[?against=<revision>]` — a line diff
  against `current` or another revision.
- `POST /config/rollback/{revision}` — re-submits that revision's exact bytes
  through the same validate-then-store path `POST /config` uses; rollback
  never rewrites history, it creates a new revision with old content.

A `gsp` instance opts in with `--controller <url>` (+ `--controller-token` if
the controller requires one) **instead of** `--config <file>` — the two are
mutually exclusive. On a controller outage `gsp` keeps running its
last-applied config and retries the subscribe connection with capped
exponential backoff (500ms → 30s); nothing about the data path pauses.

**Role (phase 12 slice 1)**: `--role standalone` (default) is everything
above unchanged. `--role slave --parent-url <url> [--parent-token <token>]`
makes this controller a relay: it never accepts a write of its own (`POST
/config` and `POST /config/rollback/*` both `403`, `"this controller is a
slave tier..."`) and instead seeds from the parent's `GET /config` at
startup, then holds a `GET /config/subscribe` connection to the parent and
re-lands every revision it receives in its own store (its own local revision
numbers — not required to match the parent's, only the config text is
shared). A `gsp` instance, or a further `slave` tier, points at *this*
controller exactly as it would at a `standalone` one — the relay is
transparent below this tier. Losing the parent freezes this tier on
last-known-good with the same reconnect-with-backoff behavior `gsp`'s own
`--controller` client has; it never promotes itself to `standalone`.

**Operator intent log (phase 12 slice 3)**: alongside `/config*`, every
`gsp-controller` also serves `POST /intent` and `GET /intent/subscribe`, a
second `sled`-backed revision log (its own `<data_dir>/intent` database)
covering the **pool-scoped** phase-5 admin verbs — backend add
(`{"op":"backend_add","pool":...,"addr":...}`), backend remove
(`backend_remove`), backend admin state (`backend_patch`, `state` one of
`enabled`/`draining`/`disabled`), and route-hint (`route_hint`,
`{"src_ip":...,"pool":...,"ttl_sec":...}`, default `ttl_sec` 30). `POST
/intent` validates the op's shape (addr/IP parse, known state) before
accepting it — there's no `gsp_config::validate()` equivalent for a bare op,
so this is that check, one hop earlier than broadcasting it. A `gsp`
instance running with `--controller` automatically also subscribes to
`/intent/subscribe` and applies every op it receives through the same
`RuntimeHandle` calls the admin API's own handlers use — an intent op is a
new *source* for an existing mutation, never a new code path. **Not part of
this log**: whole-instance drain/undrain (targets one instance, not "every
instance with pool X" — use the aggregator's targeted fan-out or the
instance's own `/admin/drain`) and resolver pins (still open, `docs/01`).
The `slave`-role write gate applies to `/intent` too, but slave-tier relay
for the intent log is not built yet — only the config log relays through a
parent today.

### `gsp-aggregator` — fleet reads and operational fan-out

One `--auth-token`-gated (optional) surface fed by every instance's own push,
plus a `--instance-token` this aggregator presents going back out to each
instance's admin API (a separate secret from `--auth-token` — one gates calls
in, the other authenticates calls out):

**Hierarchy (phase 12 slice 2)**: `--parent-url` (+ `--tier-name`, required;
`--parent-token`; `--parent-push-interval-sec`, default 10) makes this tier
also push its own merged view — every instance it currently knows,
individually, with its name rewritten `"{tier_name}/{instance}"` — up to a
parent aggregator's `POST /ingest`, on the same interval shape `gsp`'s own
`--aggregator` push uses. A parent needs no configuration to accept this: a
child aggregator's push looks exactly like a proxy's own push, just under a
namespaced instance name, so it shows up in the parent's `/fleet/*` views
with no special-casing. Namespacing only affects what's pushed upward — this
tier's own local `/fleet/*` view and `admin_url`-based fan-out still use the
un-namespaced names.

- `POST /ingest` — an instance's periodic self-reported summary (pool/backend
  health + admin state, session *counts*, its own `admin_url`). Latest-write-wins,
  **never persisted** — every fact here is a proxy's own state, re-pushed on
  the next tick, so restarting the aggregator loses nothing durable.
- `GET /fleet/pools` / `/fleet/sessions` — every known instance's ingested
  summary, each entry carrying `last_seen_ms_ago`.
- `GET /fleet/healthz` — per-instance staleness (`stale` past 30s, ~3x `gsp`'s
  default 10s push interval).
- `GET /fleet/subscribe` (SSE) — the same summaries, pushed on change
  (debounced 150ms so a burst of near-simultaneous instance pushes collapses
  into one resend, not one per push).
- `POST /fleet/instances/{instance}/drain` / `/undrain` — targeted fan-out to
  that instance's own `POST /admin/drain`/`/admin/undrain`; `404` if the
  instance isn't known.
- `POST /fleet/pools/{pool}/backends` — broadcast to every known instance's
  own `POST /pools/{pool}/backends`.
- `PATCH` / `DELETE /fleet/pools/{pool}/backends/{addr}` — broadcast to every
  instance's own `PATCH`/`DELETE /pools/{pool}/backends/{addr}`.
- `POST /fleet/route-hint` — broadcast to every instance's own
  `POST /route-hint`.

Every broadcast response is `{"results": [{"instance", "status", "body"}, ...]}`
— one entry per known instance, `status: null` (not a failed request) for one
that couldn't be reached; a broadcast never fails or blocks on one bad
instance. A `gsp` instance opts in with `--aggregator <url>` (+
`--aggregator-token`, `--aggregator-instance`, `--aggregator-interval-sec`,
default 10s) — independent of `--controller`, pushing state and pulling
config are unrelated axes.

### `gsp-ui` — the operator dashboard's BFF

A dedicated process holding both the controller's and the aggregator's own
bearer tokens on the operator's behalf; the browser only ever holds a session
cookie (`--ui-password`, `POST /ui/login`/`/ui/logout`, `GET /ui/session`),
never a bearer token. Everything else is a thin, header-preserving proxy:
`/api/fleet/*` → the `gsp-aggregator` routes above (`--aggregator-url`/
`--aggregator-token`), `/api/config*` → the `gsp-controller` routes above
(`--controller-url`/`--controller-token`), and `GET /ws/fleet` — a browser
WebSocket fed by one shared subscription to the aggregator's
`/fleet/subscribe` (one aggregator connection total, fanned out to every
connected browser, not one per tab). `--static-dir` (default
`crates/gsp-ui/web/dist`, built by `make ui`) serves the React/Vite/TS
frontend as a fallback under whatever the API routes above don't claim — it's
the one process, one port an operator's browser ever talks to. Either proxy
target is optional; fleet reads/config actions 503 cleanly if the
corresponding `--*-url` was never given.

### Fronting layer: anycast vs. L4 load balancer

| | **BGP anycast** (same VIP announced from every site) | **L4 LB / NLB** (per-region VIP, flow hashing) |
|---|---|---|
| Client → nearest site | routing decides; no extra hop | DNS / GeoDNS picks the region; one LB hop |
| Instance loss | BGP withdraw (hold-timer seconds); flows rehash to another instance | LB health check fails (1–3 intervals); flows rehash |
| Flow stability | ECMP must be **per-flow consistent** (resilient hashing) or TCP breaks on any member change | LB provides this; UDP needs "sticky" / per-flow tables |
| `transparent: true` | works only if replies return via the same instance — usually means anycast for the return path too, or SNAT | works if the LB is DSR or the backend routes replies back through the LB/instance |
| Best for | UDP-heavy, latency-critical, multi-region | single region, TCP, teams already running an NLB |

Rules of thumb: prefer **PROXY protocol over `transparent`** whenever the fronting
layer may rehash a live flow (anycast reconvergence, LB scaling) — a rehashed TCP
flow that lands on a new instance is a reconnect either way, but PROXY protocol
keeps client-IP preservation working without a symmetric return path. Keep UDP
idle timeouts short (tens of seconds) so a moved client re-establishes quickly.

### Capacity planning per instance

Size an instance by the **scarcest** of:

- **New-session rate.** Each new TCP connection / UDP session does one
  `Pool::acquire`, one upstream `connect(2)`, one task spawn (see the latency
  ledger in `HANDOVER.md`). Load-test with `gsp-bench --connections` and
  `tcpkali`; set `settings.limits.max_new_sessions_per_sec` to ~70 % of the
  measured knee and alert before it.
- **Concurrent sessions / FDs.** ~1 FD per client + 1 per upstream, plus the
  per-session buffers (2×64 KB UDP, 2×32 KB TCP). Set `max_connections` /
  `max_udp_sessions` under the file-descriptor `ulimit` with headroom; watch
  `gsp_fd_open` against `gsp_fd_limit`.
- **Bandwidth.** Single-stream throughput is near line rate (the pump is a
  buffered copy); the limit is NIC / softirq. Spread interrupts (RSS) and run
  `workers` = cores.
- **Headroom for failover.** With N instances behind the fronting layer, plan so
  any single instance loss leaves the rest below ~75 % on every axis above.

### Dashboards & alerts

Per-instance panels: `gsp_active_connections` / `gsp_active_udp_sessions`,
`gsp_listener_connections_total` rate by `result`, `gsp_bytes_total` rate,
`gsp_connection_duration_seconds` p50/p99, `gsp_fd_open` vs. `gsp_fd_limit`.
(No worker-level busy-ratio panel yet — that metric is planned, not
implemented; see above.)
Fleet roll-ups: `sum by (pool) (gsp_pool_backends{state="healthy"})`,
`sum(rate(gsp_filter_blocked_total[5m])) by (filter)`,
`sum(rate(gsp_discovery_refresh_total{result!="ok"}[15m])) by (pool,kind)`.

Alerts (in addition to the list below):

- `gsp_discovery_refresh_total{result!="ok"}` continuously for > 3 refresh
  intervals on a pool → the source is down and the pool is frozen at
  last-known-good.
- `gsp_discovery_backends` drops > 50 % between scrapes → a bad discovery result;
  cross-check against `gsp_pool_backends{state="healthy"}`.
- `count(up{job="gsp"}) < N_expected` → an instance is gone; confirm the fronting
  layer took its VIP / member out.
- `gsp_config_version` stale (not advancing) across a rollout → file-watch or
  SIGHUP not reaching that instance.

The v2 distributed control plane
([10-distributed-control-plane.md](10-distributed-control-plane.md)) removes the
"fan it out yourself" / "re-apply after restart" caveats and adds a single fleet
view + web UI.

## Capacity planning / alerts

- **Latency budget (NFR N1/N2):** `make bench` (`crates/gsp-bench`, `latency`
  mode) measures the proxy's *added* p50/p99 request→response latency on a
  single host and reports `PASS`/`MISS` vs. `< 0.5 ms` / `< 2 ms`. Run it
  before/after a change to catch per-connection overhead regressions.
- **Concurrency ramp (partial N1/N2-under-load, informational for N4/N5):**
  `cargo run --release -p gsp-bench -- --mode concurrency` spawns the real
  `gsp` binary as a separate process and ramps a real held-open connection
  count through it (`--steps`), reporting the proxy child's own RSS/fd count
  and added-latency percentiles at each step — see
  `crates/gsp-bench/README.md`. Reaches tens of thousands of connections on
  one host (capped by the client's ephemeral-port range), not the full
  500k/1M N4/N5 targets, and says nothing about N3 (loopback bandwidth
  exceeds the 20 Gbit/s target, so it can't be validated locally) or N9 (real
  HA). Aggregate throughput (N3), the full 500k conns / 1M sessions (N4/N5)
  and HA (N9) still need dedicated hardware, multiple hosts, and a real load
  generator (`tcpkali`, `wrk2`, `iperf3`).
- Alert: `gsp_pool_backends{state="healthy"} < N_min` per pool.
- Alert: `gsp_datagrams_dropped_total` rate > 0 (buffers too small / overload).
- Alert: `gsp_resolver_requests_total{result!="ok"}` share > 1%.
- Alert: `gsp_fd_open / gsp_fd_limit > 0.8`.
- Once built (see "Planned / not yet built" above): worker-busy-ratio and
  session-setup-latency alerts.
