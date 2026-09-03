# 02 – Architecture

## Overview

```
                 ┌─────────────────────────── Proxy instance ──────────────────────────┐
                 │                                                                     │
   Clients       │   ┌─────────┐   ┌──────────┐   ┌───────────┐   ┌────────────────┐   │   Backends
 (TCP/UDP)  ─────┼──▶│Listener │──▶│ Router   │──▶│ Upstream/ │──▶│ Connection/    │───┼──▶ Game servers
                 │   │ (accept)│   │ (match + │   │ Pool +    │   │ Session pump   │   │    (private network)
                 │   └─────────┘   │ resolver)│   │ LB + aff. │   │ (splice/io)    │   │
                 │        │        └────┬─────┘   └─────┬─────┘   └───────┬────────┘   │
                 │        │             │               │                │            │
                 │   ┌────┴─────────────┴───────────────┴────────────────┴────────┐   │
                 │   │                     Data plane (hot path)                  │   │
                 │   └───────────────────────────┬───────────────────────────────┘   │
                 │                               │ reads snapshot (lock-free)        │
                 │   ┌───────────────────────────┴───────────────────────────────┐   │
                 │   │  Control plane: config loader · admin API · discovery     │   │
                 │   │  adapter · health checker · metrics/log exporter          │   │
                 │   └───────────────────────────────────────────────────────────┘   │
                 └─────────────────────────────────────────────────────────────────────┘
```

## Data plane / control plane separation

- **Data plane**: accepts connections, makes the routing decision against an
  **immutable config snapshot**, pumps bytes. No lock on the hot path; snapshot swap
  via an atomic pointer swap (`arc-swap` pattern).
- **Control plane**: builds new snapshots (from file + discovery + admin API), runs
  health checks, exports metrics/logs. Runs in its own tasks/threads, may block.

A **snapshot** contains: listener definitions, the compiled routing table (including
precompiled matchers), pools with their current backend list **and** health state.
Health updates produce a new snapshot (cheap copy-on-write of the affected pool
struct, the rest is shared).

## Components

### 1. Listener
- Binds socket(s). TCP: `accept` loop or `SO_REUSEPORT` sharding across workers.
- UDP: one/more `recvmmsg` loops per socket, `SO_REUSEPORT` for scaling.
- Applies **early filters**: IP allow/deny, connection rate limit (cheap, before any
  allocation).
- Hands off to the router: for TCP after an optional peek of the first bytes; for UDP
  with the first datagram.

### 2. Router
- Selects the first matching route (deterministic priority).
- Matcher types: `always`, `sni`, `first-bytes` (prefix/regex/length), `client-cidr`,
  `dst` (destination IP/prefix), `port` (destination port), `external`.
- **Determine destination address**: with a prefix bind (one socket for a whole
  prefix), via `IP_PKTINFO`/`IPV6_RECVPKTINFO` (UDP, per datagram) or `getsockname()`
  (TCP). This is the lever for games with no protocol hint: subdomain → its own
  destination IP.
- **First-packet peek** (TCP): `MSG_PEEK` up to `peek_max_bytes` or `peek_timeout`.
  If that is not enough (the client sends nothing first), the default route applies.
- **External resolver**: calls gRPC/HTTP with `{listener, src_ip, sni, first_bytes(b64),
  routing_key}`, expects `{pool | target, sticky_key?, ttl}`. The result is cached
  (key configurable). Timeout → fallback route.
- Result: target pool + optional affinity key.

### 3. Upstream / pool
- Holds the backend list + states from the snapshot.
- The LB strategy picks a healthy backend; the affinity key is consulted first
  (consistent hashing / sticky table with TTL).
- Checks per-backend caps (max sessions, new-session rate). No candidate → defined
  error handling (TCP: RST/close; UDP: drop datagram + metric).

### 4. Connection / session pump
- **TCP**: bidirectional copy. Linux fast path via `splice()` between the two sockets
  (zero-copy), fallback to user-space buffers. Propagate half-close, timeouts (idle,
  connect), pass through `TCP_NODELAY`.
- **UDP**: one connected upstream socket per session (`connect(2)`) so replies find
  their way back without a table lookup. Session table: `HashMap<4-tuple, Session>` +
  an idle-timer timing wheel. The idle timeout closes the session.
- **PROXY protocol**: when the option is enabled, write the header before the first
  payload to the backend (TCP) or prepend it (UDP, first datagram).

### 5. Health checker (control plane)
- Active checks per backend by interval/timeout/thresholds (rise/fall).
- Passive signals from the data path (connect errors, early resets, UDP "port
  unreachable" / ICMP) feed in as events.
- Publishes a new snapshot on a state change.

### 6. Config loader & discovery adapter
- Loads/validates YAML, builds the snapshot, compiles matchers.
- Adapters (DNS SRV, Consul, K8s Endpoints, static) implement a common
  `BackendSource` interface and provide backend lists per pool.
- Reload: new snapshot becomes active atomically; listeners are re-bound only if their
  bind changed (otherwise kept running).

### 7. Admin API
- `GET /config` (active snapshot, redacted), `GET /pools`, `GET /sessions?...`
- `POST /pools/{p}/backends`, `DELETE ...`, `PATCH .../{b} {state: draining}`
- `POST /route-hint` – push resolver: `{src_ip, pool|target, ttl_sec}` for games with
  no protocol hint, set by launcher/matchmaker (scheme C in [03](03-routing.md))
- `POST /reload`, `GET /healthz`, `GET /readyz`, `GET /metrics`
- Auth: mTLS or bearer token, bound to an internal interface only.

## Threading / runtime model

- **One worker per CPU core**, `SO_REUSEPORT` sockets per worker → no accept
  contention.
- Each session is pinned to its accepting worker (thread-local session table → no
  locks in the UDP path).
- Control-plane tasks on their own small thread pool.
- Memory: per-worker pre-reserved buffer pools; session structs from a slab allocator
  to avoid fragmentation.

## Failure & error behavior

- Backend fails during a session: TCP → close both sides, metric; UDP → the session
  stays briefly (configurable), then dropped. Optionally "rehome" new datagrams to
  another backend if no affinity is required.
- Proxy instance fails: clients reconnect; an L4 LB / anycast in front of the proxy
  spreads them onto the remaining instances. No shared session state in v1.
- Overload: early rate limits + `accept` throttling + load shedding with a metric,
  before the latency of existing sessions suffers.

## Extension points

- **Sniffer plugin API**: `fn sniff(&[u8]) -> Option<RouteHint>` (SNI, Minecraft,
  Steam A2S, FiveM …). In-process (statically linked) in v1; WASM/out-of-process
  plugins later.
- **BackendSource** adapters (see above).
- **Filter chain** before routing (ACL, rate limit, geo) as an ordered, configurable
  list.
