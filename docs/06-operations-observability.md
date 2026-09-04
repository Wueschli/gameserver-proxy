# 06 – Operations & observability

## Metrics (Prometheus)

### Listener / connections
- `gsp_listener_connections_total{listener,protocol,result}` – `result` =
  `accepted|denied_acl|denied_ratelimit|no_route|resolver_error`
- `gsp_active_connections{listener,protocol}` (gauge)
- `gsp_active_udp_sessions{listener}` (gauge)
- `gsp_connection_duration_seconds{listener,pool}` (histogram)
- `gsp_session_setup_seconds{listener,pool,phase}` – `phase` = `peek|route|resolve|connect`

### Throughput
- `gsp_bytes_total{listener,pool,dir}` – `dir` = `c2s|s2c`
- `gsp_packets_total{listener,pool,dir}` (UDP) — v0 emits `{listener,dir}` only
- `gsp_datagrams_dropped_total{listener,reason}` — v0 `reason` =
  `no_route|no_backend|upstream_bind|upstream_send|outside_prefix|draining|reply_bind|first_packet_gate`
  (`outside_prefix`: prefix-mode listener, datagram destination not in `prefix`;
  `first_packet_gate`: `first_packet_gate` listener, first datagram not recognised)

### Upstream / pool
- `gsp_pool_backends{pool,state}` (gauge; `state` = healthy|unhealthy|draining|disabled)
- `gsp_backend_active_sessions{pool,backend}` (gauge)
- `gsp_backend_connect_errors_total{pool,backend,kind}` – `kind` = `timeout|refused|unreachable`
- `gsp_healthcheck_total{pool,backend,result}`
- `gsp_lb_selections_total{pool,strategy,result}` – `strategy` =
  `round_robin|least_conn|consistent_hash`; `result` = `ok|no_backend|at_capacity`
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

### Resolver
- `gsp_resolver_requests_total{resolver,result}` – `ok|empty|timeout|error`
- `gsp_resolver_cache_total{resolver,result}` – `hit|hit_negative|miss|stale|uncacheable`
- `gsp_resolver_latency_seconds{resolver}` (histogram)
- `gsp_resolver_cache{resolver,state}` – hits/misses/entries/evictions

### Proxy internals
- `gsp_config_reload_total{result}` / `gsp_config_version` (gauge, timestamp)
- `gsp_worker_busy_ratio{worker}`
- `gsp_buffer_pool_exhausted_total`
- `gsp_fd_open` / `gsp_fd_limit`
- `gsp_build_info{version,commit}`

**Added RTT** is the key SLO metric: from `session_setup_seconds` + an optional
periodic synthetic ping (proxy→backend) and, if sniffers/resolvers provide latency
data, a client→proxy estimate.

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
  `draining` / `active_conns`). Implemented (phase 5). `GET /pools`,
  `GET /sessions?listener=&src=&pool=` – introspection (`/sessions` not yet).
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
- `/sessions` is not yet implemented.

## Backend health checks

- **Active**: `tcp_connect` / `udp_probe` (send/expect bytes) / `http` (sidecar port).
  Parameters: `interval`, `timeout`, `rise`, `fall`. Checks run in the control plane,
  the result → a new snapshot.
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

- **Config distribution** today is the operator's job — bake into the image (change
  = rolling redeploy), or render the file from a store (ConfigMap /
  `consul-template` / Ansible) and let file-watch pick it up in seconds. The
  structural config is hot-reloaded; validate-before-swap keeps a bad file from
  taking an instance down.
- **Backend membership** comes from phase-8 discovery (`backend_sources`: DNS SRV
  / Consul / k8s Endpoints), not per-instance file edits. Every instance runs the
  same source config and converges independently; a source outage freezes the
  set at last-known-good rather than draining a pool fleet-wide.
- **Operator intent** (drain / add / remove a backend, route hints) is applied
  per instance via each instance's admin API and is **not persisted or
  fleet-synced today** — fan it out yourself, and re-apply after a restart.
- **`transparent: true` needs the backend's return path through the same instance**
  that owns the connection (per-flow-consistent ECMP / anycast). PROXY protocol
  has no such constraint — prefer it when the fronting layer can rehash.
- Aggregate `/metrics` and `/pools` in Prometheus; there is no built-in
  fleet-wide view.

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
  `gsp_fd_open / gsp_fd_limit`.
- **Bandwidth.** Single-stream throughput is near line rate (the pump is a
  buffered copy); the limit is NIC / softirq. Spread interrupts (RSS) and run
  `workers` = cores.
- **Headroom for failover.** With N instances behind the fronting layer, plan so
  any single instance loss leaves the rest below ~75 % on every axis above.

### Dashboards & alerts

Per-instance panels: `gsp_active_connections` / `gsp_active_udp_sessions`,
`gsp_listener_connections_total` rate by `result`, `gsp_worker_busy_ratio`,
`gsp_bytes_total` rate, `gsp_connection_duration_seconds` p50/p99.
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

- **Latency budget (NFR N1/N2):** `make bench` (`crates/gsp-bench`) measures the
  proxy's *added* p50/p99 request→response latency on a single host and reports
  `PASS`/`MISS` vs. `< 0.5 ms` / `< 2 ms`. Run it before/after a change to catch
  per-connection overhead regressions. Aggregate throughput (N3), 500k conns /
  1M sessions (N4/N5) and HA (N9) need dedicated hardware and a real load
  generator (`tcpkali`, `wrk2`, `iperf3`).
- Alert: `gsp_worker_busy_ratio > 0.8` (5 min) → scale out.
- Alert: `gsp_pool_backends{state="healthy"} < N_min` per pool.
- Alert: `gsp_datagrams_dropped_total` rate > 0 (buffers too small / overload).
- Alert: `gsp_resolver_requests_total{result!="ok"}` share > 1%.
- Alert: `gsp_fd_open / gsp_fd_limit > 0.8`.
- Alert: `session_setup_seconds` p99 over SLO.
