# 05 – Configuration

## Principles

- **One declarative YAML file** is the source of truth. Discovery adapters and the
  admin API only supplement the **backend lists** and **states** at runtime.
- **Validate before activating.** A faulty config is rejected; the running one stays
  active.
- **Hot reload** via `SIGHUP`, file watch, or `POST /reload`. Listeners are re-bound
  only when their bind address changes.
- Environment-variable interpolation (`${VAR}`) for secrets/tokens.

## Schema (reference)

> **Implemented subset (roadmap phase 2 + phase 3 routing, partial).**
> `gsp-config` currently accepts a reduced, flatter schema: `pools[].targets`
> (no `backend_sources`); `balancer: round_robin | least_conn | consistent_hash`
> (scalar, not an object) — `consistent_hash` also reads a pool-level
> `hash_on: src_ip | src_ip_port` (default `src_ip`), rejected on the other
> balancers; `health_check.type: tcp_connect | udp_probe` with `send_hex` /
> `expect_hex_prefix` for `udp_probe`; and `per_backend.max_sessions`.
>
> A listener maps to a pool either with the `pool: <name>` shorthand or with a
> priority-ordered `routes:` list — `[{ match, action }]`, first match wins,
> `action: { pool: <name> }` — but not both. Implemented matchers:
> `match: { type: always }`, `{ type: client_cidr, cidrs: [<prefix>, ...] }`
> (source IP), `{ type: dst, cidrs: [<prefix>, ...] }` (destination IP the client
> connected to; a plain listener sees only its bind IP, but a UDP listener with
> `prefix: <cidr>` serves a whole routed prefix on one `IP_PKTINFO` socket and
> replies from the address that was hit),
> `{ type: port, ports: [<int> | "lo-hi", ...] }` (destination port from the
> accepting socket),
> `{ type: first_bytes, prefix: "hex:ff.." | "ascii:..", length: { min, max } }`
> — a ≤ 512-byte prefix and/or a byte-count window on the connection's first
> bytes (TCP `MSG_PEEK` / first UDP datagram; at least one of `prefix` / `length`),
> and `{ type: sni, host: ["exact", "*.suffix", ".suffix"] }` — the `server_name`
> from the peeked (not terminated) TLS ClientHello, TCP listeners only. The
> `{ type: sniffer, sniffer: <name>, host: [...] }` matcher parses (one sniffer
> per listener) but matches nothing until a sniffer plugin is loaded — no
> sniffers are built in; the name is checked at listener start, not by
> `validate()`. The sniffer loader (Phase 9) and the `external` resolver are
> still to come; regex-over-first-bytes is a Phase 9 plugin concern, not a
> `first_bytes` sub-form.
>
> **Push resolver:** `POST /route-hint` (admin API) with
> `{ "src_ip": "...", "pool": "...", "ttl_sec": 30 }` records a short-lived
> `src_ip → pool` hint; a listener with `route_hint: true` applies it before its
> route list. `pool` must exist; `ttl_sec` 1..=3600.
>
> Listener options: `prefix: <cidr>` (UDP only) → prefix mode as above, with a
> wildcard `bind`; `freebind: true` (TCP only) → bind with `IP_FREEBIND` /
> `IPV6_FREEBIND`; `route_hint: true` → consult the push resolver. UDP listeners
> take
> `affinity: { hash_on: src_ip | src_ip_port }` (defaulting
> to `src_ip`); a UDP session reads the routed pool's `idle_timeout_sec` once
> when it is created. See `config.example.yaml`. The full schema below is the
> target.

```yaml
# global
settings:
  workers: 0                 # 0 = number of CPU cores
  admin:
    listen: "127.0.0.1:9900"
    auth: { mode: "bearer", token: "${ADMIN_TOKEN}" }   # or mode: mtls
  metrics: { path: "/metrics" }
  log:
    format: "json"
    connection_log: { enabled: true, sample: 1.0 }
  limits:
    max_connections: 500000
    max_udp_sessions: 1000000

# reusable filters
filters:
  - name: block-bogons
    type: deny_cidr
    cidrs: ["10.0.0.0/8", "192.168.0.0/16", "100.64.0.0/10"]
  - name: conn-rate
    type: rate_limit
    scope: src_ip            # src_ip | src_/24 | global
    new_per_sec: 50
    burst: 100

# backend sources
backend_sources:
  - name: static-eu
    type: static
    targets: ["10.1.0.11:7777", "10.1.0.12:7777"]
  - name: k8s-match
    type: kubernetes_endpoints
    namespace: "games"
    service: "match-server"
    port_name: "game"
  - name: srv-us
    type: dns_srv
    record: "_game._udp.us.internal.example.com"
    refresh_sec: 10

# pools
pools:
  - name: match-eu
    source: static-eu
    balancer: { strategy: consistent_hash, hash_on: src_ip }
    affinity: { table_ttl_sec: 120 }
    health_check:
      type: udp_probe          # tcp_connect | udp_probe | http
      send_hex: "01"
      expect_hex_prefix: "02"
      interval_sec: 2
      timeout_ms: 500
      rise: 2
      fall: 3
    per_backend:
      max_sessions: 200
      max_new_per_sec: 20
    proxy_protocol: "none"     # none | v1 | v2 | v2-udp
    connect_timeout_ms: 300
    idle_timeout_sec: 90

  - name: match-us
    source: srv-us
    balancer: { strategy: least_conn }
    health_check: { type: tcp_connect, interval_sec: 2, timeout_ms: 400, rise: 2, fall: 3 }

  - name: source-query
    source: static-eu
    balancer: { strategy: round_robin }
    idle_timeout_sec: 10

# resolvers (external routing logic)
resolvers:
  - name: matchmaker
    type: grpc                 # grpc | http
    endpoint: "https://matchmaker.internal:8443"
    timeout_ms: 40
    cache:
      key: ["first_bytes:0:16"]     # e.g. a session-token prefix
      positive_ttl_sec: 30
      negative_ttl_sec: 2
      max_entries: 200000
    on_error: "reject"         # reject | fallback | stale_ok

# listeners + routes
listeners:
  - name: public-udp
    bind: "0.0.0.0:7777"
    protocol: udp
    filters: ["block-bogons", "conn-rate"]
    peek_max_bytes: 64
    routes:
      - match: { type: always }
        action: { resolver: matchmaker }
      - match: { type: always }        # fallback if resolver on_error=fallback
        action: { pool: match-eu }

  - name: public-tls
    bind: "0.0.0.0:443"
    protocol: tcp
    peek_max_bytes: 2048
    peek_timeout_ms: 200
    routes:
      - match: { type: sni, suffix: ".eu.example.com" }
        action: { pool: match-eu }
      - match: { type: sni, suffix: ".us.example.com" }
        action: { pool: match-us }
      - match: { type: always }
        action: { pool: match-eu }

  - name: public-mc
    bind: "0.0.0.0:25565"
    protocol: tcp
    peek_max_bytes: 512
    routes:
      - match: { type: sniffer, name: minecraft, host: "survival.example.net" }
        action: { pool: match-eu }
      - match: { type: always }
        action: { pool: match-us }

  - name: source-27015
    bind: "0.0.0.0:27015"
    protocol: udp
    routes:
      - match: { type: first_bytes, prefix: "hex:FFFFFFFF" }
        action: { pool: source-query }
      - match: { type: always }
        action: { pool: match-eu }

  # Raw UDP with no protocol hint: subdomain by destination IP (see docs/03 scheme A)
  - name: raw-udp
    bind: "[2001:db8:ace:1::]/64:7777"   # prefix bind, one socket
    protocol: udp
    recv_dst_addr: true                   # enable IPV6_RECVPKTINFO / IP_PKTINFO
    freebind: true                        # ip_nonlocal_bind / IP_FREEBIND
    routes:
      - match: { type: dst, cidr: "2001:db8:ace:1::1/128" }   # survival.example.net
        action: { pool: match-eu }
      - match: { type: dst, cidr: "2001:db8:ace:1::2/128" }   # creative.example.net
        action: { pool: match-us }
      - match: { type: always }
        action: { reject: true }          # unknown destination IP -> drop
    affinity: { hash_on: src_ip }

  # IPv4-only variant: subdomain by port (scheme B), SRV hands out the port
  - name: raw-udp-v4
    bind: "0.0.0.0:30000-30099"
    protocol: udp
    routes:
      - match: { type: port, eq: 30001 }
        action: { pool: match-eu }
      - match: { type: port, eq: 30002 }
        action: { pool: match-us }
      - match: { type: always }
        action: { reject: true }
```

## Validation rules (excerpt)

- Every `pool.source` must point to a `backend_sources[].name`.
- Every `action.pool` / `action.resolver` must exist.
- Every listener needs at least one route; the last route should be `always`
  (otherwise a "no default" warning).
- `proxy_protocol: v2-udp` only together with `protocol: udp`.
- `consistent_hash` requires `hash_on`.
- `match.type: dst` requires `recv_dst_addr: true` on the listener (otherwise the
  destination address per packet/connection is unknown); a prefix bind requires
  `freebind: true` and a prefix routed to the host.
- `match.type: port` is only meaningful with a range bind (`:30000-30099`).
- Bind addresses must not overlap between listeners (same IP:port:proto); a prefix
  bind must not cover a single bind address of another listener.
- Numeric ranges: timeouts > 0, `rise`/`fall` ≥ 1, TTLs ≥ 0.

## Reload semantics

| Change | Behavior |
|--------|----------|
| Backend added/removed | immediately in the new snapshot, existing sessions untouched |
| Backend → `draining` | no new sessions, existing ones drain |
| Pool balancer changed | applies to **new** routing decisions |
| Route changed/added | applies to new connections/sessions |
| Listener bind changed | the old socket is closed, the new one bound (brief gap) |
| `settings.workers` changed | requires a restart (documented) |
| Invalid file | reload rejected, metric `config_reload_failed_total++`, old config stays active |
