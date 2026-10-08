# 06 – Operations & observability

## Metrics (Prometheus)

### Listener / connections
- `wayhouse_listener_connections_total{listener,result}` – `result` =
  `accepted|no_route|sniffer_reject` (`sniffer_reject`: a `sniffer` sniffer
  returned a `reject` hint for the first bytes; the connection is dropped
  before routing). ACL / rate-limit / geo / cap rejections do **not** get a
  `result` value here — they increment `wayhouse_filter_blocked_total` instead
  (see "Security / filter chain" below), and a resolver miss/error is folded
  into `no_route` rather than its own value. No `protocol` label — split by
  `listener` name instead (a listener has one fixed protocol).
- `wayhouse_active_connections{listener}` (gauge) — no `protocol` label.
- `wayhouse_active_udp_sessions{listener}` (gauge)
- `wayhouse_connection_duration_seconds{listener}` (histogram) — no `pool` label
  (a connection's pool can change across a reload; the listener is stable).
- `wayhouse_session_setup_seconds{listener,phase}` — **planned, not implemented**;
  see "Planned / not yet built" below.

### Throughput
- `wayhouse_bytes_total{listener,dir}` – `dir` = `c2s|s2c`. No `pool` label.
- `wayhouse_packets_total{listener,dir}` (UDP). No `pool` label.
- `wayhouse_datagrams_dropped_total{listener,reason}` — v0 `reason` =
  `no_route|no_backend|upstream_bind|upstream_send|outside_prefix|draining|reply_bind|first_packet_gate|sniffer_reject|pending_full`
  (`outside_prefix`: prefix-mode listener, datagram destination not in `prefix`;
  `first_packet_gate`: `first_packet_gate` listener, first datagram not recognised;
  `sniffer_reject`: a `sniffer` sniffer returned a `reject` hint — no session, no reply;
  `pending_full`: a new session's external resolver call is still in flight and the
  per-worker pending cap (1024 sessions, 1 MiB buffered) or the per-session buffer (4 datagrams) is full)

### Upstream / pool
- `wayhouse_pool_backends{pool,state}` (gauge; `state` = healthy|unhealthy|draining|disabled)
- `wayhouse_backend_active_sessions{pool,backend}` (gauge)
- `wayhouse_backend_connect_errors_total{pool,backend,kind}` – `kind` = `timeout|refused|unreachable`
- `wayhouse_healthcheck_total{pool,backend,result}`
- `wayhouse_lb_selections_total{pool,strategy,result}` – `strategy` =
  `round_robin|least_conn|consistent_hash|weighted`; `result` = `ok|no_backend|at_capacity`
- `wayhouse_route_hints_applied_total{listener}` – a `POST /route-hint` entry decided
  routing for a connection / UDP session
- `wayhouse_proxy_protocol_headers_total{pool,version}` – `version` = `v1|v2|v2-udp`; a
  PROXY protocol header was prepended to an upstream connection (TCP) or the first
  datagram of a session (UDP `v2-udp`). `pool="(resolver target)"` for a pool-less
  resolver `target` connection (the header form comes from the resolver's
  `proxy_protocol:`)

### Backend discovery (phase 8)
- `wayhouse_discovery_refresh_total{pool,kind,result}` – `kind` =
  `dns_srv|consul|kubernetes|tunnel`; `result` = `ok|empty|error|withdrawn`. One
  per refresh attempt. `empty` / `error` keep the last-known-good backend set (the
  pool is never cleared by a failed refresh). `withdrawn` (`tunnel` only) means the
  controller deleted or expired an origin this proxy had seen: the pool's set is
  cleared.
- `wayhouse_discovery_backends{pool}` (gauge) – addresses returned by the pool's
  source at its last successful refresh.

### Security / filter chain
- `wayhouse_accept_errors_total{listener}` (counter, #174) – a TCP `accept` failed,
  typically `EMFILE` (out of file descriptors). The
  worker pauses 50 ms, doubling to 1 s, until an accept succeeds, and logs at most
  one `accept failed` warning per second with a `suppressed` count; this counter
  carries the full rate.
- `wayhouse_filter_blocked_total{listener,filter}` – `filter` = `acl` | `geo` |
  `rate_ip` | `rate_net` | `src_conn_ip` | `src_conn_net` | `max_conn` |
  `max_udp` | `max_new_rate`; a connection / new UDP session was dropped by the
  pre-routing filter chain — `acl` = source-IP allow/deny, `geo` = GeoIP country
  allow/deny (or the DB failed to load and the listener fails closed), `rate_ip`
  / `rate_net` = the per-listener per-IP / per-/24 (per-/64) token bucket was
  empty, `src_conn_ip` / `src_conn_net` = the per-listener concurrent per-source
  connection / session cap was reached, `max_conn` / `max_udp` / `max_new_rate` =
  a process-wide `settings.limits` cap was hit

### Sniffers (phase 9)
- `wayhouse_sniffer_calls_total{name,result}` – `result` = `ok` | `unrecognised` |
  `timeout` | `trap` | `bad_output`; one increment per `sniff()` call through
  the WASM loader. `timeout` = the epoch-interruption deadline
  (`settings.sniffers.call_timeout_ms`) fired; `trap` = any other WASM trap or
  an instantiation failure; `bad_output` = the sniffer returned a result the
  host couldn't decode as a `RouteHint`.
- `wayhouse_sniffer_call_seconds{name}` (histogram) – wall-clock time of one
  `sniff()` call, instantiation included.

### Resolver
- `wayhouse_resolver_requests_total{resolver,result}` – `ok|empty|timeout|error`
- `wayhouse_resolver_cache_total{resolver,result}` – `hit|hit_negative|miss|stale|uncacheable`

### Proxy internals
- `wayhouse_config_reload_total{result}` / `wayhouse_config_version` (gauge, timestamp)
- `wayhouse_listener_bind_failures_total{listener}` – a reload could not (re)bind the
  listener (port taken by another process, no permission, bad address). The previous
  listener, if any, keeps running; every later reload retries (even of an unchanged
  file). Startup refuses to run
  instead.
- `wayhouse_build_info{component,version,commit}` (gauge, always `1`) — set once at
  startup; the same label set on `wayhouse` (`component="wayhouse"`) and every fleet
  binary. `commit` is a 12-char git SHA baked in at build time
  (`crates/wayhouse-http/build.rs`): the `WAYHOUSE_GIT_SHA` environment variable if set
  (the Docker build arg, since the build has no `.git`), else `git rev-parse`,
  else `"unknown"` (e.g. a source tarball).
- `wayhouse_fd_open` (gauge, no labels) — this process's open file descriptor count
  (`/proc/self/fd` on Linux; absent elsewhere), sampled every 5 s by a small
  background task (`crates/wayhouse/src/procinfo.rs`), independent of the
  health-check sweep.
- `wayhouse_fd_limit` (gauge, no labels) — this process's `RLIMIT_NOFILE` soft
  limit (`getrlimit`, via `nix`), sampled once at startup (it doesn't change
  at runtime).
- `wayhouse_tls_handshakes_refused_total{reason="per_source"|"rate"}` (counter) — TLS
  connections to the admin API closed at the door: the source already had its cap
  of handshakes in flight, or was opening connections faster than its rate.
  Counted in `wayhouse-http` (its name lives in `wayhouse_http::tls`, not `metrics_defs.rs`,
  which `wayhouse-http` cannot depend on). Exposed on `wayhouse`'s `/metrics` and on the
  fleet binaries' (below). Present only when that listener serves TLS.
- `wayhouse_tls_handshakes_evicted_total` (counter, no labels) — pending handshakes
  dropped to make room at the global cap (same notes).
- `wayhouse_gossip_members` (gauge, no labels) — current SWIM member count in this
  instance's Tier-2 gossip mesh (phase 13, `docs/10` "Tier 2", `wayhouse-core::
  gossip`). Present only when `settings.gossip` is set.
- `wayhouse_gossip_messages_total{direction="sent"|"received"}` (counter, phase
  13) — gossip datagrams that passed HMAC verification.
- `wayhouse_gossip_auth_rejected_total` (counter, no labels, phase 13) — gossip
  datagrams dropped for a missing/invalid HMAC tag; never trusted, never
  forwarded to the SWIM state machine.
- `wayhouse_gossip_stale_rejected_total` (counter, no labels) — authentic gossip
  datagrams dropped because their sender timestamp is more than 30 s from this
  node's clock (a replay, or an instance with a skewed clock; keep NTP running).
- `wayhouse_gossip_version_rejected_total` (counter, no labels, #185) — authentic,
  fresh gossip datagrams dropped because their version byte is not this build's
  gossip version: a peer on an incompatible release. Dropped before decoding; the
  first one per process is logged as a warning.
- `wayhouse_protocol_mismatch_total{route_group="controller"|"aggregator"|"raft"|"proxy"}`
  (counter, #185) — component requests refused with `426` because the caller's
  `X-Wayhouse-Protocol` major differs, is not `<major>.<minor>`, or (on component
  routes) is missing. Emitted by the controller, aggregator and proxy; see
  [10](10-distributed-control-plane.md) "Versioning".
- `wayhouse_backend_domain_down{pool,backend}` (gauge, 0/1, phase 13) — whether
  the Tier-2 domain quorum is currently overriding this backend to down.
  Independent of, and unable to clear, the backend's own local `healthy`
  flag (`wayhouse_pool_backends`'s `healthy`/`unhealthy` counts already reflect
  the combined result).

**Added RTT**: no built-in RTT SLO metric exists yet — `make bench`'s added
p50/p99 (vs. NFR N1/N2) is the closest thing today, measured out-of-band, not
exported as a `/metrics` series. See "Planned / not yet built" below.

### Planned / not yet built

Documented here as real future work, not implemented — none of these exist in
`metrics_defs.rs` today, so don't expect them on `/metrics` yet:

- **`wayhouse_resolver_latency_seconds{resolver}`**, **`wayhouse_session_setup_seconds{listener,phase}`**,
  **`wayhouse_worker_busy_ratio{worker}`** — plausible finer-grained latency /
  saturation instrumentation, deferred until a real debugging need shows the
  existing aggregate metrics (`wayhouse_resolver_requests_total`,
  `wayhouse-bench`'s added-latency numbers) aren't enough to explain a slowdown.
  Not worth the hot-path cost speculatively.
- ~~`wayhouse_resolver_cache{resolver,state}`~~ / ~~`wayhouse_buffer_pool_exhausted_total`~~
  — dropped from the plan: the former was a duplicate of
  `wayhouse_resolver_cache_total` above (typo'd as a second metric), the latter
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
   a client whose resolver answer pointed at it is re-resolved once the cached
   answer expires.
3. Wait until `wayhouse_backend_active_sessions` = 0 **or** `drain_deadline` is reached.
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
   dropped (`wayhouse_datagrams_dropped_total{reason="draining"}`).
   A **config reload** that replaces a UDP listener behaves differently: the old
   workers keep answering their live sessions, but they leave the kernel's
   `SO_REUSEPORT` flow hash immediately, so new flows (and, on their next datagram,
   the clients of those old sessions, who get a new session under the new config)
   go to the replacement group and nothing is dropped as `draining`.

## Multi-instance operations (HA)

Production is many identical, **independent** `wayhouse` instances across hosts / AZs /
regions, fronted by **anycast or an L4 load balancer**. No shared data-plane
state (ADR 4): lose an instance and the LB / anycast spreads its clients onto the
rest; affected sessions reconnect.

- **Config distribution**: either the file-based approach below, or the phase
  10+11 controller (see "Fleet control plane" below) — bake into the image
  (change = rolling redeploy), render the file from a store (ConfigMap /
  `consul-template` / Ansible) and let file-watch pick it up in seconds, or
  point every instance at a `wayhouse-controller` with `--controller <url>`. Either
  way the structural config is hot-reloaded; validate-before-swap keeps a bad
  config from taking an instance down (the controller rejects it at
  submission time instead, one hop earlier — see below).
- **Backend membership** comes from phase-8 discovery (`backend_sources`: DNS SRV
  / Consul / k8s Endpoints), not per-instance file edits. Every instance runs the
  same source config and converges independently; a source outage freezes the
  set at last-known-good rather than draining a pool fleet-wide.
- **Operator intent** (drain / add / remove a backend, route hints) is still
  applied per instance, either directly against that instance's admin API or
  fanned out through `wayhouse-aggregator` (see below), and is **not persisted or
  replayed after a restart** — moving intent into the controller's revision
  log is phase 12, not this release.
- **`transparent: true` needs the backend's return path through the same instance**
  that owns the connection (per-flow-consistent ECMP / anycast). PROXY protocol
  has no such constraint — prefer it when the fronting layer can rehash.
- Aggregate `/metrics` and `/pools` yourself in Prometheus for per-instance detail;
  for a single fleet-wide read/operate surface, see `wayhouse-aggregator` and
  `wayhouse-ui` below — there is still no fleet-wide metrics aggregation.

## Fleet control plane (phase 10+11)

Three additional, independent binaries. Originally a single-tier PoC
(phase 10+11); the `docs/10` `standalone`/`slave` hierarchy, intra-tier HA,
adoption, staged/canary rollout, RBAC, and moving operator intent into the
controller's revision log are phase 12 — **all built**, documented per-flag in
the subsections below. All three serve unauthenticated
`GET /healthz` for liveness regardless of their auth settings below.

**`GET /metrics` on the fleet binaries.** Each serves a Prometheus endpoint
(`wayhouse_http::metrics`) with `wayhouse_build_info{component,version,commit}` and the TLS handshake
counters above. It is gated like the rest of the binary's API: on `wayhouse-controller` and
`wayhouse-aggregator` by `--auth-token` (`Authorization: Bearer`, open when no token is set),
unless a dedicated `--metrics-token` (at least 16 bytes) is given: then `/metrics`
accepts only that token (not the admin token) and the token opens nothing else, so
Prometheus never holds the admin secret. `wayhouse-ui` has only `--metrics-token`, because a
scraper cannot hold the UI's browser session. A `wayhouse-ui` with a login configured and no
`--metrics-token` does not serve `/metrics` at all; an open UI (no login) serves it
open. Per-request or per-store metrics for these binaries are not built; the
per-instance proxy surface stays on each `wayhouse`.

### `wayhouse-controller` — structural config distribution

One `--auth-token`-gated (optional) surface, backed by an embedded `sled`
store that never forgets a revision (ADR 20):

- `POST /config` — submit a raw YAML config (the same document a `wayhouse
  --config` file would hold). Validated with the same `wayhouse_config::parse_str`
  a file reload runs; a rejected submission (`422`, JSON error body) leaves
  the current revision untouched — the revision only advances on success.
  Response body is `{"revision": N}` (the new revision only appears here, not
  as a header).
- `GET /config` — the current revision's raw text, with `X-Config-Revision`
  header. `404` before any submission.
- `GET /config/subscribe?since=<revision>` (SSE) — catch-up range then a live
  tail; `wayhouse --controller <url>` consumes this to hot-reload. A lagging
  subscriber just re-runs the catch-up query against the store — no delivery
  state kept on the writer side.
- `GET /config/revisions` — history (revision, size, whether it's `current`).
- `GET /config/revisions/{revision}` — a past revision's raw text.
- `GET /config/revisions/{revision}/diff[?against=<revision>]` — a line diff
  against `current` or another revision.
- `POST /config/rollback/{revision}` — re-submits that revision's exact bytes
  through the same validate-then-store path `POST /config` uses; rollback
  never rewrites history, it creates a new revision with old content.

A `wayhouse` instance opts in with `--controller <url>` (+ `--controller-token` if
the controller requires one) **instead of** `--config <file>` — the two are
mutually exclusive. On a controller outage `wayhouse` keeps running its
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
shared). A `wayhouse` instance, or a further `slave` tier, points at *this*
controller exactly as it would at a `standalone` one — the relay is
transparent below this tier. Losing the parent freezes this tier on
last-known-good with the same reconnect-with-backoff behavior `wayhouse`'s own
`--controller` client has; it never promotes itself to `standalone`.

**Operator intent log (phase 12 slice 3)**: alongside `/config*`, every
`wayhouse-controller` also serves `POST /intent` and `GET /intent/subscribe`, a
second `sled`-backed revision log (its own `<data_dir>/intent` database)
covering the **pool-scoped** phase-5 admin verbs — backend add
(`{"op":"backend_add","pool":...,"addr":...}`), backend remove
(`backend_remove`), backend admin state (`backend_patch`, `state` one of
`enabled`/`draining`/`disabled`), and route-hint (`route_hint`,
`{"src_ip":...,"pool":...,"ttl_sec":...}`, default `ttl_sec` 30). `POST
/intent` validates the op's shape (addr/IP parse, known state) before
accepting it — there's no `wayhouse_config::validate()` equivalent for a bare op,
so this is that check, one hop earlier than broadcasting it. A `wayhouse`
instance running with `--controller` automatically also subscribes to
`/intent/subscribe` and applies every op it receives through the same
`RuntimeHandle` calls the admin API's own handlers use — an intent op is a
new *source* for an existing mutation, never a new code path. **Not part of
this log**: whole-instance drain/undrain (targets one instance, not "every
instance with pool X" — use the aggregator's targeted fan-out or the
instance's own `/admin/drain`) and resolver pins (still open, `docs/01`).
The `slave`-role write gate applies to `/intent` too, and (phase 12 slice 4)
a `slave` tier also relays the intent log from its parent, the same way it
relays config — a `slave` controller now holds a full copy of both of its
parent's logs while still never originating either one itself.

**Intra-tier HA (phase 12 slice 6)**: `--ha-node-id <id>` + `--ha-peers
id=host:port,...` (identical on every replica; an entry may instead be
`id=https://host[:port]`, see docs/12 "HA replicas over TLS") turn a `standalone` tier into
an `N`-node Raft group (embedded `openraft`) replicating both the config and
intent logs together. `POST /config` and `POST /intent` propose a Raft entry
and only return once it's committed; a replica that isn't the current leader
transparently forwards the request to whichever one is (never a redirect —
every existing client stays unaware HA exists). Reads (`GET
/config`/`/config/subscribe`/`/intent/subscribe`/etc.) are served by
whichever replica gets the request, straight from its own local copy — not
a linearizable read, a deliberate relaxation justified in `docs/10`
"Intra-tier HA (design)". `--ha-token` gates the new peer-only `/raft/*`
routes (`append`, `vote`, `snapshot`) — a separate secret from
`--auth-token`. **HA combines with `--role slave`** (2026-10-05): only the
Raft leader subscribes to the parent and proposes each relayed revision into
the group; the relay cursor (the highest parent revision absorbed, one per
log) is replicated with the log, so a newly elected leader resumes where the
group left off and a duplicate proposal is skipped. Start every replica with
the same `--role slave --parent-url ...`. `POST /admin/adopt` is refused on an
HA controller (the role is a per-node setting).

**Staged / canary rollout (phase 12 slice 7)**: `POST /config?stage=canary
&group=<name>` submits a revision visible only to a subscriber reporting
that group; a plain `POST /config` (no `stage`) is unaffected — immediately
visible to everyone, exactly as before this feature existed. `GET
/config?group=<name>` and `GET /config/subscribe?since=<revision>&group=
<name>` show what's visible to that group: the highest revision that's
either promoted or staged to it. `POST /config/promote/{revision}` makes an
existing revision visible to everyone (`404` if it doesn't exist) — this is
the one operation that changes an existing revision rather than creating a
new one. `GET /config/revisions` gained `promoted`/`canary_groups` fields
per entry. One group per submission in this release. Not available for
`/intent` (config only).

**Adoption (phase 12 slice 5)**: `POST /admin/adopt
{"parent_url":"...","parent_token":"..."}` flips a running `standalone`
tier to `slave` without a restart. Refused with `409` unless this tier's
config *and* intent stores are both still empty (a tier with existing
history must be replaced by a fresh one to join a hierarchy, per `docs/10`
"Adoption") or it's already a slave. On success, seeds from the new
parent's current config and starts relaying both logs, exactly like a
`--role slave` boot — the response body's `seeded_config_revision` is the
parent's revision number seeded from (`null` if the parent's config log was
empty at that moment). Same `--auth-token` gate as `/config*`/`/intent*`.

### `wayhouse-aggregator` — fleet reads and operational fan-out

One `--auth-token`-gated (optional) surface fed by every instance's own push,
plus a `--instance-token` this aggregator presents going back out to each
instance's admin API (a separate secret from `--auth-token` — one gates calls
in, the other authenticates calls out):

**Hierarchy (phase 12 slice 2)**: `--parent-url` (+ `--tier-name`, required;
`--parent-token`; `--parent-push-interval-sec`, default 10) makes this tier
also push its own merged view — every instance it currently knows,
individually, with its name rewritten `"{tier_name}/{instance}"` — up to a
parent aggregator's `POST /ingest`, on the same interval shape `wayhouse`'s own
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
- `GET /fleet/healthz` — per-instance staleness (`stale` past 30s, ~3x `wayhouse`'s
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

Every broadcast response is `{"results": [{"instance", "status", "error", "detail"?}, ...]}`
— one entry per known instance, `status: null` (not a failed request) for one
that couldn't be reached; an instance that refuses (non-2xx) also carries
`detail`, the first 200 bytes of its reply; a broadcast never fails or blocks on one bad
instance. A `wayhouse` instance opts in with `--aggregator <url>` (+
`--aggregator-token`, `--aggregator-instance`, `--aggregator-interval-sec`,
default 10s, and `--aggregator-admin-url`, the base URL the aggregator should
fan out to when `http(s)://<settings.admin.listen>` is not reachable from it,
e.g. in a container, behind NAT or a TLS terminator) — independent of `--controller`, pushing state and pulling
config are unrelated axes.

### `wayhouse-ui` — the operator dashboard's BFF

A dedicated process holding both the controller's and the aggregator's own
bearer tokens on the operator's behalf; the browser only ever holds a session
cookie (`POST /ui/login`/`/ui/logout`, `GET /ui/session`), never a bearer
token. Everything else is a thin, header-preserving proxy:
`/api/fleet/*` → the `wayhouse-aggregator` routes above (`--aggregator-url`/
`--aggregator-token`), `/api/config*` → the `wayhouse-controller` routes above
(`--controller-url`/`--controller-token`), and `GET /ws/fleet` — a browser
WebSocket fed by one shared subscription to the aggregator's
`/fleet/subscribe` (one aggregator connection total, fanned out to every
connected browser, not one per tab). An open socket does not outlive its
session: it re-checks the session every 30 s and before each outgoing message,
and closes with code 1008 after logout, idle timeout, max age or eviction (the
check doesn't count as activity, so a tab left open still idles out). `--static-dir` (default
`crates/wayhouse-ui/web/dist`, built by `make ui`) serves the React/Vite/TS
frontend as a fallback under whatever the API routes above don't claim — it's
the one process, one port an operator's browser ever talks to. Either proxy
target is optional; fleet reads/config actions 503 cleanly if the
corresponding `--*-url` was never given.

**RBAC and audit (phase 12 slice 8, `docs/10` "RBAC and audit (design)")**:
two login modes, mutually exclusive.

- **`--ui-password <secret>`** (legacy, unchanged from phase 10+11) — one
  shared secret, `POST /ui/login {"password":"..."}`, every session
  implicitly `admin`.
- **`--users-file <path>`** — multi-operator accounts, a YAML list of
  `{username, password_hash, role}` (`role` one of `viewer`/`operator`/
  `admin`); login is `POST /ui/login {"username":"...","password":"..."}`.
  `password_hash` is an argon2 PHC string — `wayhouse-ui --hash-password` reads a
  password from stdin and prints one, the intended way to populate an entry
  (never put a plaintext password in the file).

With neither flag, the UI is fully open and every session is implicitly
`admin` — same posture every other optional-auth surface in this fleet has.

Sessions expire: `--session-idle-timeout-secs` (default 1800) and
`--session-max-age-secs` (default 43200, also the cookie's `Max-Age`), with at
most `--max-sessions` (default 1000) held at once. `POST /ui/login` is
rate-limited per client address and per username (`429` + `Retry-After`).

Three roles gate three route groups: `viewer` (every `GET` — fleet reads,
config/revision reads/diffs, `GET /ws/fleet`), `operator` (+ the phase-5
intent verbs — drain/undrain, backend add/patch/delete, route-hint),
`admin` (+ config submit/rollback/promote). A session below a route's
minimum role gets `403` (distinct from `401`, which means no valid session
at all).

**Audit**: every write `wayhouse-ui` proxies carries an `X-Actor: <username>`
header (`--ui-password` mode never sets it — there's no per-session
identity there). `wayhouse-controller` records it per revision — `GET
/config/revisions`' new `actor` field is the durable half of this audit
trail. `wayhouse-aggregator` logs `(instance/pool, actor, verb)` via `tracing`
for the fan-out verbs and forwards the header on to each instance — a
convenience, not a durable record (this aggregator holds no durable state
by design).

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
  ledger in `HANDOVER.md`). Load-test with `wayhouse-bench --connections` and
  `tcpkali`; set `settings.limits.max_new_sessions_per_sec` to ~70 % of the
  measured knee and alert before it.
- **Concurrent sessions / FDs.** ~1 FD per client + 1 per upstream, plus the
  per-session buffers (2×64 KB UDP, 2×32 KB TCP). Set `max_connections` /
  `max_udp_sessions` under the file-descriptor `ulimit` with headroom; watch
  `wayhouse_fd_open` against `wayhouse_fd_limit`.
- **Bandwidth.** Single-stream throughput is near line rate (the pump is a
  buffered copy); the limit is NIC / softirq. Spread interrupts (RSS) and run
  `workers` = cores.
- **Headroom for failover.** With N instances behind the fronting layer, plan so
  any single instance loss leaves the rest below ~75 % on every axis above.

### Dashboards & alerts

Per-instance panels: `wayhouse_active_connections` / `wayhouse_active_udp_sessions`,
`wayhouse_listener_connections_total` rate by `result`, `wayhouse_bytes_total` rate,
`wayhouse_connection_duration_seconds` p50/p99, `wayhouse_fd_open` vs. `wayhouse_fd_limit`.
(No worker-level busy-ratio panel yet — that metric is planned, not
implemented; see above.)
Fleet roll-ups: `sum by (pool) (wayhouse_pool_backends{state="healthy"})`,
`sum(rate(wayhouse_filter_blocked_total[5m])) by (filter)`,
`sum(rate(wayhouse_discovery_refresh_total{result!="ok"}[15m])) by (pool,kind)`.

Alerts (in addition to the list below):

- `wayhouse_discovery_refresh_total{result!="ok"}` continuously for > 3 refresh
  intervals on a pool → the source is down and the pool is frozen at
  last-known-good.
- `wayhouse_discovery_backends` drops > 50 % between scrapes → a bad discovery result;
  cross-check against `wayhouse_pool_backends{state="healthy"}`.
- `count(up{job="wayhouse"}) < N_expected` → an instance is gone; confirm the fronting
  layer took its VIP / member out.
- `wayhouse_config_version` stale (not advancing) across a rollout → file-watch or
  SIGHUP not reaching that instance.

The v2 distributed control plane
([10-distributed-control-plane.md](10-distributed-control-plane.md)) removes the
"fan it out yourself" / "re-apply after restart" caveats and adds a single fleet
view + web UI.

## Capacity planning / alerts

- **Latency budget (NFR N1/N2):** `make bench` (`crates/wayhouse-bench`, `latency`
  mode) measures the proxy's *added* p50/p99 request→response latency on a
  single host and reports `PASS`/`MISS` vs. `< 0.5 ms` / `< 2 ms`. Run it
  before/after a change to catch per-connection overhead regressions.
- **Concurrency ramp (partial N1/N2-under-load, informational for N4/N5):**
  `cargo run --release -p wayhouse-bench -- --mode concurrency` spawns the real
  `wayhouse` binary as a separate process and ramps a real held-open connection
  count through it (`--steps`), reporting the proxy child's own RSS/fd count
  and added-latency percentiles at each step — see
  `crates/wayhouse-bench/README.md`. Reaches tens of thousands of connections on
  one host (capped by the client's ephemeral-port range), not the full
  500k/1M N4/N5 targets, and says nothing about N3 (loopback bandwidth
  exceeds the 20 Gbit/s target, so it can't be validated locally) or N9 (real
  HA). Aggregate throughput (N3), the full 500k conns / 1M sessions (N4/N5)
  and HA (N9) still need dedicated hardware, multiple hosts, and a real load
  generator (`tcpkali`, `wrk2`, `iperf3`).
- Alert: `wayhouse_pool_backends{state="healthy"} < N_min` per pool.
- Alert: `wayhouse_datagrams_dropped_total` rate > 0 (buffers too small / overload).
- Alert: `wayhouse_resolver_requests_total{result!="ok"}` share > 1%.
- Alert: `wayhouse_fd_open / wayhouse_fd_limit > 0.8`.
- Once built (see "Planned / not yet built" above): worker-busy-ratio and
  session-setup-latency alerts.
