# 05 – Configuration

## Principles

- **One declarative YAML file** is the source of truth. Discovery adapters and the
  admin API only supplement the **backend lists** and **states** at runtime.
- **Validate before activating.** A faulty config is rejected; the running one stays
  active.
- **Hot reload** via `SIGHUP` or file watch. Listeners are reconciled by name: an
  added listener is spawned, a removed one is stopped, and one whose definition
  changed (bind, protocol, routes, …) is stopped and re-spawned. A
  listener whose definition is unchanged keeps running untouched. `backend_sources`
  discovery refresh tasks are reconciled the same way (added / removed /
  re-parameterised → started / stopped / restarted).
- Environment-variable interpolation (`${VAR}`) for secrets/tokens.

## Schema (reference)

> **Implemented subset (roadmap phase 2 + phase 3 routing, partial).**
> `gsp-config` currently accepts a reduced, flatter schema: `pools[].targets`
> or a `pools[].source` naming a `backend_sources[]` entry
> (`static` / `dns_srv` / `consul` / `kubernetes` / `tunnel`, flat fields,
> `refresh_interval_sec`); `balancer: round_robin | least_conn | consistent_hash
> | weighted` (scalar, not an object) — `consistent_hash` also reads a pool-level
> `hash_on: src_ip | src_ip_port` (default `src_ip`), and `weighted` a pool-level
> `weights: { "ip:port": N }` map (weight `>= 1`, default 1), each rejected on the
> other balancers; `health_check.type: tcp_connect | udp_probe | none` with
> `send_hex` / `expect_hex_prefix` for `udp_probe` (see "Health checks" below);
> `per_backend.max_sessions`; and
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
> `settings.workers`). Modules within an already-configured `dir` can also be
> managed over HTTP: `GET/POST /admin/sniffers` + `DELETE
> /admin/sniffers/{name}` on the instance's own admin API (`409` if
> `settings.sniffers` is absent), fanned out fleet-wide via `gsp-aggregator`'s
> `POST/DELETE /fleet/sniffers[/{name}]` and the admin GUI's Plugins page —
> see `crates/plugins/README.md` "Installing over HTTP instead of `cp`".
>
> **Tier-2 regional health fabric** (phase 13, docs/10 "Tier 2" — built):
> `settings.failure_domain` (a string identity — this instance's
> reachability-equivalence class, e.g. an AZ or region) and
> `settings.gossip: { bind, seeds?, quorum_fraction?, psk }` must be set
> together, or not at all — `validate()` rejects either one alone. `bind` /
> `seeds[]` are UDP socket addresses; `seeds` is optional (any subset of a
> domain's live members is enough to join, membership then discovers the
> rest) and defaults to empty. `quorum_fraction` (default 0.66) must be
> `> 0.5` and `<= 1.0` — the fraction of the domain that must report a
> backend down before that verdict can override this instance's own "up"
> reading; a same-or-under-half quorum could contradict itself between two
> overlapping majorities, hence the `> 0.5` floor. `psk` (a pre-shared key,
> must not be empty) authenticates every gossip datagram with an
> HMAC-SHA256 tag. When the mesh itself lands, it will only ever be able to
> push a backend's flag *down*, never revive it — a backend returns healthy
> only on this instance's own local `rise` streak, and a Tier-1
> `force-down` (`AdminState::Disabled`) always wins over the domain view.
> Startup-only, like `settings.workers`.
>
> **Fleet grouping:** `settings.group` (optional string) is this instance's
> self-reported organization path — e.g. `"eu/frankfurt/cluster-a"` — pushed
> to gsp-aggregator alongside its `IngestPayload` so the admin GUI can render
> a grouped/tree fleet view. `/`-separated, non-empty segments, no
> leading/trailing `/`; `validate()` rejects anything else. Purely a display
> label — never consulted by routing/forwarding, and independent of
> `failure_domain` (which is about health-fabric membership, not fleet
> organization).
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
> A UDP session reads the routed pool's `idle_timeout_sec` once when it is
> created. UDP affinity (a client that comes back after its session was evicted
> reaches the same backend) comes from the pool: use `balancer: consistent_hash`
> with `hash_on: src_ip | src_ip_port`. There is no per-listener `affinity` key
> and no sticky table (removed in #56: it was per worker, so `SO_REUSEPORT` kept
> only ~34% of clients on 4 workers). A `round_robin` pool gives a returning
> client a new backend. See `config.example.yaml`.
>
> **Admin API auth** (phase 10+11 slice 10): `settings.admin.auth_token`
> (a flat string, not the target schema's `auth: { mode, token }` object
> below) gates every admin API request except `GET /healthz` with a bearer
> check. `None` (the default) leaves the API open — network-boundary-only
> auth, same as always. A single shared secret, not RBAC/mTLS — appropriate
> for gating a control-plane API that's meant to stay behind its own network
> boundary regardless; needed once something calls in from outside that
> boundary, which today means `gsp-aggregator`'s intent-verb fan-out
> (`docs/10` "The aggregator"), given the same token to present.
>
> **Admin API TLS** (2026-10-02): `settings.admin.tls: { cert, key }` (PEM chain,
> leaf first; PEM private key — both required) serves the admin API over HTTPS.
> Startup-only like `listen`; the files are re-read every 30 s, so a renewed
> certificate needs no restart. `gsp --check` loads the pair; errors name
> `settings.admin.tls.cert`/`.key`.
> Optional keys under `tls` bound the TLS handshakes: `max_pending` (512),
> `max_pending_per_source` (16), `new_per_source_per_sec` (20; `0` = no rate limit),
> `new_per_source_burst` (64); see docs/12 "Native TLS". The admin URL reported to `--aggregator` becomes
> `https://<listen>`, so the certificate must be valid for the `listen` address
> (an IP SAN), and the aggregator needs `--ca-file` for a private CA. With
> `--controller` every instance gets the same YAML, so the paths are the same on every
> host while each host's file must hold a certificate for **its own** `listen`
> address. See docs/12 "TLS for the fleet services".
>
> The full schema below is the target.

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
    type: kubernetes            # LIST .../endpointslices for the service, plus a watch for instant updates
    service: "match-server"
    namespace: "games"          # default: "default"
    port_name: "game"           # optional; else each slice's first port
    api: "https://kubernetes.default.svc"   # default; SA token + CA read in-pod
    refresh_interval_sec: 10   # k8s: resync interval; changes arrive via the watch
  - name: consul-eu
    type: consul               # GET /v1/health/service/<service>?passing=true, plus a blocking query for instant updates
    service: "match-server"
    consul_addr: "http://127.0.0.1:8500"    # default
    tag: "prod"                # optional
    consul_token_file: "/run/secrets/consul-token"   # optional ACL token (X-Consul-Token), re-read as it rotates
    refresh_interval_sec: 10   # consul: resync interval; changes arrive via the blocking query
  - name: srv-us
    type: dns_srv              # resolves the SRV record; port from the record
    record: "_game._udp.us.internal.example.com"
    refresh_interval_sec: 10
  - name: home-origin
    type: tunnel               # phase 14 (built) — the `tunnel` BackendSource.
                                # Resolves backend addresses a `gsp-agent`-managed
                                # origin behind a WireGuard tunnel has registered
                                # with the controller's backend-peers registry.
    pubkey: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="  # origin's WG pubkey
    refresh_interval_sec: 10

# pools
pools:
  - name: match-eu
    source: static-eu
    balancer: { strategy: consistent_hash, hash_on: src_ip }
    affinity: { table_ttl_sec: 120 }
    health_check:
      type: udp_probe          # tcp_connect | udp_probe | none | http
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
    bind: "[::]:7777"                     # wildcard bind, one socket
    protocol: udp
    prefix: "2001:db8:ace:1::/64"         # enables IP_PKTINFO / IPV6_RECVPKTINFO;
                                           # routes by the real per-datagram destination
    routes:
      - match: { type: dst, cidrs: ["2001:db8:ace:1::1/128"] }   # survival.example.net
        action: { pool: match-eu }
      - match: { type: dst, cidrs: ["2001:db8:ace:1::2/128"] }   # creative.example.net
        action: { pool: match-us }
      # No `always`/catch-all route and no `reject` action — there isn't one;
      # an unmatched destination IP simply has no matching route and is
      # dropped (no_route).

  # IPv4-only variant: subdomain by port (scheme B), SRV hands out the port.
  # One listener, one `bind: "host:lo-hi"` port range (F1.4) — one real socket
  # per port, all sharing this listener's routes; the `port` route matcher
  # picks the pool by the port the datagram actually arrived on. Ports in the
  # range with no matching route (anything but 30001/30002 here) are dropped.
  - name: raw-udp-v4
    bind: "0.0.0.0:30000-30099"
    protocol: udp
    routes:
      - match: { type: port, ports: [30001] }
        action: { pool: match-eu }
      - match: { type: port, ports: [30002] }
        action: { pool: match-us }
```

A `kubernetes` source reads the service's EndpointSlices (`discovery.k8s.io/v1`,
selected by the `kubernetes.io/service-name` label) and merges them: ready
endpoints only (an unset `ready` counts as ready), duplicates across slices
collapsed, `FQDN` slices skipped, and a slice without the wanted port ignored.
Its service account needs `list` and `watch` on `endpointslices` in the
namespace; [`deploy/k8s/45-gsp-rbac.yaml`](../deploy/k8s/45-gsp-rbac.yaml) is a
ready-made Role. Without `watch` the source logs one warning per outage and keeps
converging on the poll interval, so a missing grant slows updates but never breaks
discovery. The watch resumes from the version of the last list and reopens itself
when the API server ends it; a `410 Gone` triggers an immediate list, which renews
the version. A failing watch retries every 5 s, doubling up to 60 s. The
service-account token is read from its file again every 60 s, so the kubelet's
rotation of the bound token (about hourly) is followed without a restart.

A `consul` source lists the passing instances of the service and then holds a
blocking query (`?index=<X-Consul-Index>&wait=300s`) on the index of its last
list; a new index triggers a list at once, and the poll interval stays as the
resync net. An index that goes backwards, or a response without one, is treated as
a reset and gets a fresh list. A failing query retries like the Kubernetes watch
(5 s doubling to 60 s). With ACLs enabled, put the token in a file and point
`consul_token_file` at it; the file (not the token) is what the YAML and
`GET /config` carry, and it is read again every 60 s. The token needs
`service:read` on the service and `node:read` on the nodes running it. The service
name and the tag are URL-escaped.

Both pushed signals are coalesced: after the first change the source is fetched
once it has been quiet for 500 ms (at most 5 s after the first change), so a
rolling update of many pods costs one fetch, not one per event.

## Validation rules (excerpt)

- A pool needs exactly one of `targets:` / `source:`; `source` must point to a
  `backend_sources[].name`. `refresh_interval_sec >= 1`. `dns_srv` needs
  `record`; `consul` / `kubernetes` need `service` (`consul_token_file`, if
  given, must not be empty); `tunnel` needs `pubkey`
  (a base64-encoded 32-byte WireGuard key).
- Every `action.pool` / `action.resolver` must exist.
- Every listener needs at least one route. There is currently no warning for a
  route list with no trailing `always` — an unmatched connection/datagram is
  silently dropped (`no_route`); a "did you forget a catch-all route?" lint is
  a plausible small future addition, not built today.
- `proxy_protocol: v2-udp` only together with `protocol: udp` (for a pool, and
  for a resolver whose routes are on UDP listeners); v1/v2 only with TCP.
- A resolver's `timeout_ms`, `target_connect_timeout_ms` and
  `target_idle_timeout_sec` must all be `> 0`.
- `transparent: true` (TCP or UDP) may not be combined with `prefix`, and may not be set
  on a listener whose `pool:` or a route's pool action uses a `tunnel` backend source (the
  client and tunnel address families can differ). A resolver route can still return such a
  pool at runtime; there the proxy connects without the client's source address.
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
- `settings.failure_domain` and `settings.gossip` must both be set, or
  neither; `gossip.bind` / each `gossip.seeds[]` entry must be a valid socket
  address; `quorum_fraction` must be `> 0.5` and `<= 1.0`; `psk` must not be
  empty.
- `match.type: dst` needs the destination address to be known: on TCP it comes
  from `getsockname()` for free; on UDP it needs either `transparent: true`
  (any dest) or `prefix: <cidr>` (dest within that prefix) on the listener —
  there is no separate `recv_dst_addr` flag, `prefix`/`transparent` themselves
  turn on the recv path. `freebind` is TCP-only and is rejected together with
  `prefix` (a UDP prefix listener needs the routed prefix reachable to the
  host, not `IP_FREEBIND`).
- `match.type: port` matches the destination port of the accepting socket; its
  own `ports:` list supports a `"lo-hi"` range.
- A listener's `bind` is either one `host:port` socket address, or a
  `host:lo-hi` port range (F1.4) — one real socket per port (TCP: one per
  worker per port; UDP: same, `SO_REUSEPORT`-shared), all sharing that
  listener's routes/filters/pool selection; a route's `port` matcher still
  sees the real port a connection/datagram arrived on. Capped at 1024 ports
  per range (a typo like `0-65535` would otherwise try to open tens of
  thousands of sockets). Mutually exclusive with `prefix` (which needs
  exactly one wildcard socket).
- Bind addresses must not overlap between listeners (same IP:port:proto,
  checked per port of a range too); a prefix bind must not cover a single
  bind address of another listener.
- Numeric ranges: timeouts > 0, `rise`/`fall` ≥ 1, TTLs ≥ 0.

## Reload semantics

| Change | Behavior |
|--------|----------|
| Backend added/removed (file **or** `POST`/`DELETE /pools/{p}/backends`) | immediately in the new snapshot, existing sessions untouched. Admin add/remove edits are kept in a runtime overlay and re-applied on every file reload (the file can't silently undo them). |
| Backend → `draining` / `disabled` (`PATCH .../{addr}`) | no new sessions, existing ones drain; state carried across a reload by address |
| Pool balancer changed | applies to **new** routing decisions |
| Route changed/added | applies to new connections/sessions |
| `resolvers:` changed | live — the reload task rebuilds the resolver clients and swaps the whole set in atomically when (and only when) `resolvers:` actually differs; an in-flight resolver call finishes against the old client, new calls use the new one. A rebuild resets each resolver's LRU result cache, so expect a brief cache-cold window. A bad endpoint keeps the previous set (logged). |
| `backend_sources:` changed | live — the reload task reconciles the discovery refresh tasks (`SourceManager`): a `backend_sources[]` entry added / removed / re-parameterised (or a `pools[].source` re-pointed) starts / stops / restarts its task. A restarted task does an immediate first fetch, so the stale-set window is one round-trip. A pool that loses its `source` drops its cached discovered set (it now serves its file `targets`). A source that fails to rebuild is logged and skipped — the pool keeps its last-known-good set. |
| Listener added / removed / changed | reconciled by name at runtime — added spawned, removed stopped, changed (bind / protocol / routes / …) stopped and re-spawned. `SO_REUSEPORT` means a same-bind rebind has no gap; new sockets bind before the old ones are torn down. |
| `settings.shutdown_grace_sec` changed | live (read per shutdown) |
| `settings.workers` changed | requires a restart (documented) |
| `settings.limits.*` changed | requires a restart — the live counters / token bucket are built once at startup (like `workers`) |
| `settings.admin.*` changed (`listen`, `auth_token`, `tls`) | requires a restart — the admin API is bound once at startup; a reload silently keeps the old values. With `tls` set, the certificate **files** are still re-read every 30 s, so renewal needs no restart |
| `settings.geo_db` changed | requires a restart — the MaxMind DB is opened once at startup (a listener's `geo` codes are reloadable, the DB path is not) |
| `settings.sniffers.dir` contents changed (module added / removed / recompiled), or a `modules[].config` string changed | live — rescanned on every reload (phase 9 slice 4) and swapped in like the snapshot |
| `settings.sniffers` block added / removed, or `call_timeout_ms` / `max_memory_bytes` changed | requires a restart — the `wasmtime::Engine` and its epoch-ticker thread are built once at startup, like `settings.workers` |
| Invalid file | reload rejected, metric `config_reload_failed_total++`, old config stays active |

## Health checks

- `health_check.type` defaults to `tcp_connect`, which can never succeed against a
  UDP-only backend. A pool that only UDP listeners route to and that leaves `type`
  out is therefore **rejected** at load: pick `udp_probe` (with `send_hex`, and
  `expect_hex_prefix` if the reply is recognisable) or `none`. A pool shared with a
  TCP listener keeps the default; an explicit `type` is always accepted.
- `none` runs no active probe, for a game with no probe payload. Health then comes
  from passive observations alone: a UDP ICMP port-unreachable, a failed upstream
  send or a TCP connect failure counts against a backend, and a backend marked
  unhealthy is given another chance after one `interval_sec` (re-admitted after
  `rise` such intervals), since nothing else could ever bring it back. A truly dead
  backend therefore flaps: down on passive failures, back up after `rise` intervals,
  down again on the next failure.
- For UDP, a successful `send()` is **not** a passive success (it succeeds whether or
  not anything listens). The first reply datagram of a session is.
- A proxy-side shortage (`EMFILE`, `ENFILE`, `ENOMEM`, `ENOBUFS`, `EADDRNOTAVAIL`)
  while opening a socket, for a probe or for a session, is not counted for or
  against the backend.
- Probes run in the background, at most 256 at once; the first check of each
  backend is spread over one `interval_sec`. A slow probe does not delay the
  others, and a backend is never probed twice at once.
- `idle_timeout_sec` must be greater than 0 (as for the other timeouts).
