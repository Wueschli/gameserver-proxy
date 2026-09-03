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
  `no_route|no_backend|upstream_bind|upstream_send|outside_prefix`
  (`outside_prefix`: prefix-mode listener, datagram destination not in `prefix`)

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
  datagram of a session (UDP `v2-udp`)

### Security / filter chain
- `gsp_filter_blocked_total{listener,filter}` – `filter` = `acl`; a connection /
  new UDP session was dropped by the pre-routing filter chain on its source IP

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

## Capacity planning / alerts

- Alert: `gsp_worker_busy_ratio > 0.8` (5 min) → scale out.
- Alert: `gsp_pool_backends{state="healthy"} < N_min` per pool.
- Alert: `gsp_datagrams_dropped_total` rate > 0 (buffers too small / overload).
- Alert: `gsp_resolver_requests_total{result!="ok"}` share > 1%.
- Alert: `gsp_fd_open / gsp_fd_limit > 0.8`.
- Alert: `session_setup_seconds` p99 over SLO.
