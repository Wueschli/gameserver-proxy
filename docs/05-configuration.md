# 05 – Configuration

## Principles

- **One declarative YAML file** is the source of truth. Discovery adapters and the
  admin API only supplement the **backend lists** and **states** at runtime.
- **Validate before activating.** A faulty config is rejected; the running one stays
  active.
- **Hot reload** via `SIGHUP` or file watch. Listeners are reconciled by name: an
  added listener is spawned, a removed one is stopped, and one whose definition
  changed (bind, protocol, routes, affinity, …) is stopped and re-spawned. A
  listener whose definition is unchanged keeps running untouched.
- Environment-variable interpolation (`${VAR}`) for secrets/tokens.

## Schema (reference)

> **Implemented subset (roadmap phase 2 + phase 3 routing, partial).**
> `gsp-config` currently accepts a reduced, flatter schema: `pools[].targets`
> or a `pools[].source` naming a `backend_sources[]` entry
> (`static` / `dns_srv` / `consul` / `kubernetes`, flat fields,
> `refresh_interval_sec`); `balancer: round_robin | least_conn | consistent_hash
> | weighted` (scalar, not an object) — `consistent_hash` also reads a pool-level
> `hash_on: src_ip | src_ip_port` (default `src_ip`), and `weighted` a pool-level
> `weights: { "ip:port": N }` map (weight `>= 1`, default 1), each rejected on the
> other balancers; `health_check.type: tcp_connect | udp_probe` with `send_hex` /
> `expect_hex_prefix` for `udp_probe`; `per_backend.max_sessions`; and
> `proxy_protocol: none | v1 | v2 | v2-udp` (scalar). `v1`/`v2` are TCP-only,
> `v2-udp` UDP-only; the form must match the transport of the listeners that
> statically route to the pool (validated).
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
> `validate()`. The sniffer loader is Phase 9; regex-over-first-bytes is a Phase
> 9 plugin concern, not a `first_bytes` sub-form.
>
> **External resolver** (phase 4, slices 1–4): a top-level `resolvers:` list of
> `{ name, type: http|grpc, endpoint, timeout_ms, on_error: reject|fallback_route|stale_ok,
> proxy_protocol?, target_connect_timeout_ms?, target_idle_timeout_sec?, cache? }`
> and a route `action: { resolver: <name> }` (exactly
> one of `pool` / `resolver` per action). The proxy `POST`s `{listener, src, dst,
> sni?, first_bytes_b64, routing_key?}` and expects `{pool?, target?,
> sticky_key?, ttl_sec?}` — `target` ("ip:port") wins over `pool` and connects
> straight to that instance (no pool / health / cap); `sticky_key` is not yet
> used. `proxy_protocol: none | v1 | v2 | v2-udp` (default `none`) is the PROXY
> protocol header to prepend to a `target` connection — there is no pool to read
> it from — so the backend still sees the real client IP; a resolver-chosen
> *pool* uses that pool's own `proxy_protocol`. Same transport rule as pools:
> v1/v2 need TCP listeners on that resolver's routes, v2-udp needs UDP.
> `target_connect_timeout_ms` (default 300) / `target_idle_timeout_sec` (default
> 90) are the connect / idle timeouts for a `target` connection or UDP session —
> again, no pool to read them from; a resolver-chosen *pool* uses its own.
> `fallback_route` continues the route list on failure; `reject` drops;
> `stale_ok` serves the last (expired) cached answer if there is one, else
> drops. Optional `cache: { key: [<part>, ...], positive_ttl_sec,
> negative_ttl_sec, max_entries }` — a TTL'd LRU keyed by the joined parts
> (`src_ip` / `src_ip_port` / `sni` / `routing_key` / `first_bytes:a:b`); a
> response `ttl_sec` overrides `positive_ttl_sec`; a request missing a key part
> bypasses the cache. For `type: grpc` the `endpoint` is an `http://host:port`
> URI and the contract is `proto/resolver.proto`
> (`gsp.resolver.v1.Resolver/Resolve`).
>
> **Push resolver:** `POST /route-hint` (admin API) with
> `{ "src_ip": "...", "pool": "...", "ttl_sec": 30 }` records a short-lived
> `src_ip → pool` hint; a listener with `route_hint: true` applies it before its
> route list. `pool` must exist; `ttl_sec` 1..=3600.
>
> Listener options: `prefix: <cidr>` (UDP only) → prefix mode as above, with a
> wildcard `bind`; `freebind: true` (TCP only) → bind with `IP_FREEBIND` /
> `IPV6_FREEBIND`; `transparent: true` (TCP or UDP, Linux) → TPROXY mode:
> `IP_TRANSPARENT` on the listen socket, the original destination read per
> connection / datagram, and the real client address bound as the upstream
> source (needs `CAP_NET_ADMIN`; mutually exclusive with `prefix`);
> `route_hint: true` → consult the push resolver.
>
> **Filter chain (phase 7):** `allow: [<cidr>, …]` / `deny: [<cidr>, …]` on a
> listener (TCP or UDP) are checked against the client source IP before routing.
> `deny` is checked first and wins; a non-empty `allow` makes the listener
> default-deny for any source it does not cover. A blocked connection / new UDP
> session is dropped silently (no error reply — no reflection) and counted by
> `gsp_filter_blocked_total{listener,filter="acl"}`. Established UDP sessions are
> not re-checked per datagram. `allow` / `deny` are compiled to a radix trie, so
> large block lists (bogons + a threat feed) match in bounded time.
>
> `rate_limit: { per_ip: { rate, burst }, per_net: { rate, burst } }` (at least
> one of `per_ip` / `per_net`) is a token bucket on *new* connections / *new* UDP
> sessions, checked right after the ACL. `rate` is permits/second sustained,
> `burst` the bucket capacity (defaults to `rate`). `per_net` aggregates by the
> client's /24 (IPv4) or /64 (IPv6). A permit is taken only when every configured
> bucket can afford it; excess is dropped silently and counted by
> `gsp_filter_blocked_total{listener,filter="rate_ip"|"rate_net"}`. Bucket state
> is per proxy instance (size it per node behind anycast HA).
>
> **Per-source concurrent cap:** `per_source: { max_per_ip, max_per_net }` (at
> least one) bounds how many connections / UDP sessions are *live at once* from
> one client IP / one /24 (v4) / /64 (v6) — where `rate_limit` bounds the *rate*
> of new ones. Checked after `rate_limit`, before allocation; over the cap ⇒
> silent drop + `gsp_filter_blocked_total{listener,filter="src_conn_ip"|"src_conn_net"}`.
> The slot is released when the connection closes / the UDP session idles out.
>
> **GeoIP filter:** `geo: { allow: [CC], deny: [CC] }` on a listener (ISO 3166-1
> alpha-2 country codes, case-insensitive) is checked on the client source IP
> right after the CIDR ACL. Same precedence as `allow`/`deny`: `deny` wins, a
> non-empty `allow` is default-deny, and an IP the database can't place is
> admitted only when there is no `allow` list. Requires `settings.geo_db` — the
> path to a MaxMind Country `.mmdb` (e.g. MaxMind's free GeoLite2-Country);
> `--check` and startup fail if it can't be opened, and a listener whose DB
> somehow isn't loaded fails closed. Blocked ⇒
> `gsp_filter_blocked_total{listener,filter="geo"}`. Startup-only, like
> `settings.workers`.
>
> **Sniffer plugin loader** (phase 9; config schema slice 2, the `wasmtime`
> loader slice 3, live `dir` rescanning slice 4 — all landed):
> `settings.sniffers: { dir, call_timeout_ms, max_memory_bytes, modules: [{
> name, sha256, config? }] }`. Absent ⇒ no plugins load and a `sniffer:` route
> never matches. `dir` is a directory of `*.wasm` modules, scanned at startup and
> rescanned on every reload (added modules load, removed ones drop, changed
> ones recompile — swapped in atomically, no listener restart); `call_timeout_ms`
> (default 20) bounds a plugin's wall-clock time per call via `wasmtime` epoch
> interruption; `max_memory_bytes` (default 16 MiB) caps a call's instance
> memory; `modules:` optionally pins each module's SHA-256 for supply-chain
> verification — when non-empty, a `dir` entry not listed there (or whose hash
> doesn't match) is refused (and, on a rescan, keeps the previous plugin set
> rather than applying a half-updated one). `modules[].config` is an optional
> opaque string handed to that plugin on every `sniff` call (the plugin parses
> it itself — e.g. a match pattern for `regex-firstbytes`); a module needs a
> `modules[]` entry (hence its `sha256`) to carry a `config`. `validate()`
> rejects an empty `dir`, a zero `call_timeout_ms` / `max_memory_bytes`, a
> non-64-hex-char `sha256`, and an empty `config` string. `call_timeout_ms` / `max_memory_bytes` themselves, and the
> `settings.sniffers` block appearing/disappearing, are startup-only — the
> `wasmtime::Engine` and its epoch-ticker thread are built once (like
> `settings.workers`).
>
> **Global caps:** `settings.limits: { max_connections, max_udp_sessions,
> max_new_sessions_per_sec }` are process-wide ceilings (all optional; omit for no
> cap). `max_connections` / `max_udp_sessions` bound the live counts across every
> listener; `max_new_sessions_per_sec` is a token bucket (burst = the rate) over
> new connections **and** new UDP sessions combined. A new connection / session
> that would breach a cap is dropped before it is allocated (existing ones keep
> running) and counted by
> `gsp_filter_blocked_total{filter="max_conn"|"max_udp"|"max_new_rate"}`.
> Startup-only, like `settings.workers`.
>
> **UDP first-packet gate:** `first_packet_gate: true` on a UDP listener makes it
> create a session only when the first datagram is positively recognised — a
> `sniffer` hint that is not `reject`, or a `first_bytes` route on the listener
> that matches the datagram. Applied before the `route_hint` lookup (a spoofable
> `src_ip` hint must not bypass it). Unrecognised datagrams are dropped with no
> session and no reply, counted by
> `gsp_datagrams_dropped_total{reason="first_packet_gate"}`. Requires at least
> one `first_bytes` route or a `sniffer` (else it would drop everything).
>
> UDP listeners
> take
> `affinity: { hash_on: src_ip | src_ip_port }` (defaulting
> to `src_ip`); a UDP session reads the routed pool's `idle_timeout_sec` once
> when it is created. See `config.example.yaml`. The full schema below is the
> target.

```yaml
# global
settings:
  workers: 0                 # 0 = number of CPU cores
  shutdown_grace_sec: 30     # wait this long for in-flight conns on SIGTERM
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
    max_new_sessions_per_sec: 50000   # new conns + UDP sessions/s (burst = rate)

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

# backend sources (phase 8) — a pool takes its backends from `targets:` OR a
# named `source:`, never both. Level-triggered: the adapter returns the current
# address set; the runtime diffs it against the live set. A refresh that errors
# or returns empty keeps the last-known-good set. `static` is folded into the
# pool's targets at load time; the dynamic kinds each get one control-plane
# refresh task (never on the data path).
backend_sources:
  - name: static-eu
    type: static
    targets: ["10.1.0.11:7777", "10.1.0.12:7777"]
  - name: k8s-match
    type: kubernetes            # polls GET .../endpoints/<service>
    service: "match-server"
    namespace: "games"          # default: "default"
    port_name: "game"           # optional; else the subset's first port
    api: "https://kubernetes.default.svc"   # default; SA token + CA read in-pod
    refresh_interval_sec: 10
  - name: consul-eu
    type: consul               # GET /v1/health/service/<service>?passing=true
    service: "match-server"
    consul_addr: "http://127.0.0.1:8500"    # default
    tag: "prod"                # optional
    refresh_interval_sec: 10
  - name: srv-us
    type: dns_srv              # resolves the SRV record; port from the record
    record: "_game._udp.us.internal.example.com"
    refresh_interval_sec: 10

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
    proxy_protocol: "none"     # none | v1 | v2 | v2-udp — header for a `target` result
    target_connect_timeout_ms: 300  # connect / idle timeout for a `target` result
    target_idle_timeout_sec: 90     #   (no pool to read them from)
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

- A pool needs exactly one of `targets:` / `source:`; `source` must point to a
  `backend_sources[].name`. `refresh_interval_sec >= 1`. `dns_srv` needs
  `record`; `consul` / `kubernetes` need `service`.
- Every `action.pool` / `action.resolver` must exist.
- Every listener needs at least one route; the last route should be `always`
  (otherwise a "no default" warning).
- `proxy_protocol: v2-udp` only together with `protocol: udp` (for a pool, and
  for a resolver whose routes are on UDP listeners); v1/v2 only with TCP.
- A resolver's `timeout_ms`, `target_connect_timeout_ms` and
  `target_idle_timeout_sec` must all be `> 0`.
- `transparent: true` (TCP or UDP) may not be combined with `prefix`.
- Every entry in a listener's `allow` / `deny` must be a valid CIDR.
- `rate_limit`, if present, needs at least one of `per_ip` / `per_net`, each with
  `rate >= 1`.
- `per_source`, if present, needs at least one of `max_per_ip` / `max_per_net`,
  each `>= 1`.
- Every `settings.limits.*` value, if present, must be `>= 1` (0 would block all
  traffic — omit the key for no cap).
- `first_packet_gate: true` is UDP-only and needs at least one `first_bytes`
  route or a `sniffer` on the listener.
- A listener `geo` filter requires `settings.geo_db`, a non-empty `allow` or
  `deny`, and 2-letter country codes; the DB file must open at startup / `--check`.
- `consistent_hash` requires `hash_on`.
- `weights` is accepted only with `balancer: weighted`; every key must parse as
  `ip:port` and every value must be `>= 1`.
- `settings.sniffers.dir` must not be empty; `call_timeout_ms` /
  `max_memory_bytes` must be `>= 1`; each `modules[].sha256` must be a 64-char
  hex digest and `name` must not be empty; a `modules[].config`, if given, must
  be non-empty.
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
| Backend added/removed (file **or** `POST`/`DELETE /pools/{p}/backends`) | immediately in the new snapshot, existing sessions untouched. Admin add/remove edits are kept in a runtime overlay and re-applied on every file reload (the file can't silently undo them). |
| Backend → `draining` / `disabled` (`PATCH .../{addr}`) | no new sessions, existing ones drain; state carried across a reload by address |
| Pool balancer changed | applies to **new** routing decisions |
| Route changed/added | applies to new connections/sessions |
| `resolvers:` changed | live — the reload task rebuilds the resolver clients and swaps the whole set in atomically when (and only when) `resolvers:` actually differs; an in-flight resolver call finishes against the old client, new calls use the new one. A rebuild resets each resolver's LRU result cache, so expect a brief cache-cold window. A bad endpoint keeps the previous set (logged). |
| `backend_sources:` changed | requires a restart — the refresh tasks are spawned once at startup (like `settings.workers`). A `pools[].source` re-*pointing* at an existing source name still takes effect on reload; only adding / removing / re-parameterising a `backend_sources[]` entry needs a restart. |
| Listener added / removed / changed | reconciled by name at runtime — added spawned, removed stopped, changed (bind / protocol / routes / affinity / …) stopped and re-spawned. `SO_REUSEPORT` means a same-bind rebind has no gap; new sockets bind before the old ones are torn down. |
| `settings.shutdown_grace_sec` changed | live (read per shutdown) |
| `settings.workers` changed | requires a restart (documented) |
| `settings.limits.*` changed | requires a restart — the live counters / token bucket are built once at startup (like `workers`) |
| `settings.geo_db` changed | requires a restart — the MaxMind DB is opened once at startup (a listener's `geo` codes are reloadable, the DB path is not) |
| `settings.sniffers.dir` contents changed (module added / removed / recompiled), or a `modules[].config` string changed | live — rescanned on every reload (phase 9 slice 4) and swapped in like the snapshot |
| `settings.sniffers` block added / removed, or `call_timeout_ms` / `max_memory_bytes` changed | requires a restart — the `wasmtime::Engine` and its epoch-ticker thread are built once at startup, like `settings.workers` |
| Invalid file | reload rejected, metric `config_reload_failed_total++`, old config stays active |
