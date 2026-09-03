# HANDOVER

State of the work, decisions already made, and how to pick it up.
Last updated: 2026-09-03 (**phases 0–6 complete; phase 7 slices 1–3 done** —
filter chain: per-listener `allow` / `deny` CIDR lists + a per-listener
`rate_limit` token bucket (per source IP and per /24 / /64) + process-wide
`settings.limits` caps (`max_connections` / `max_udp_sessions` /
`max_new_sessions_per_sec`); blocked traffic dropped silently +
`gsp_filter_blocked_total{filter="acl"|"rate_ip"|"rate_net"|"max_conn"|"max_udp"|"max_new_rate"}`).
Phase 5:
`enabled` / `draining` / `disabled` backend states + `PATCH /pools/{p}/backends/{addr}`;
tracked connection draining with `shutdown_grace_sec` on SIGINT/SIGTERM;
`POST /admin/drain` + `GET /config`; runtime backend CRUD; runtime listener
add/remove/rebind. `GET /sessions` is the only deferred bit.
Phase 6 slices 1–2: per-pool `proxy_protocol: none | v1 | v2 | v2-udp` prepends a
PROXY protocol header to the upstream TCP connection (v1/v2) or the first
datagram of each UDP session (v2-udp) — `gsp_core::proxy_protocol`.
Phase 6 slices 3–4 (**phase 6 complete**): `transparent: true` on a TCP **or
UDP** listener = Linux TPROXY — `IP_TRANSPARENT` on the listen socket, the
original destination read per connection / datagram (TCP `getsockname`; UDP
`IP_RECVORIGDSTADDR` cmsg), a client-`ip:port`-bound `IP_TRANSPARENT` upstream
socket, and (UDP) a per-session `IP_TRANSPARENT` reply socket bound to the
original destination. socket2 bumped 0.5 → 0.6 for `IPV6_TRANSPARENT`.

---

## TL;DR

- **Planning docs** (`docs/00`–`09`) are complete and in English. They are the design
  source of truth.
- **Code**: Cargo workspace, roadmap **phases 0–3 complete**. Phase 3 shipped
  (slices 1–9): per-listener route rule list; `first_bytes` `prefix` + `length`;
  `consistent_hash` balancer; `sni` matcher; `dst` matcher; UDP `prefix:`
  listener + TCP `freebind:`; the sniffer API **seam** + `sniffer` matcher (no
  built-in sniffers); the `POST /route-hint` push resolver. **Phase 4**:
  `resolvers:` config + `action: { resolver: <name> }`; a `Resolver` trait +
  async `resolve_route` in `gsp-core`; `HttpResolver` (reqwest) **and
  `GrpcResolver` (tonic)** in `gsp`; `Routed::Pool` **and `Routed::Target`** (a
  pool-less connect — `proxy::handle_tcp_target` / a guard-less UDP session);
  `on_error: reject | fallback_route | stale_ok`; a `CachedResolver` TTL'd LRU
  result cache with a configurable key. The proxy forwards **TCP and
  UDP** end to end with health checks (`tcp_connect` + `udp_probe`), three
  balancers, per-backend caps, worker-local UDP session tables with `src_ip`
  affinity, hot reload, and address / first-bytes / SNI / push-hint / external
  routing — including one wildcard `IP_PKTINFO` socket serving a whole routed
  UDP prefix.
- **Phase 5 slice 1 done**: `AdminState` (`Enabled` / `Draining` / `Disabled`)
  on `Backend`; `Draining` / `Disabled` are excluded from new-session selection
  (`acquire_for` and the UDP-affinity `acquire_addr`) while live `BackendGuard`s
  keep running; state is carried across a reload by address (like health);
  `PATCH /pools/{pool}/backends/{addr} {state}` in `gsp/src/admin.rs`;
  `GET /pools` shows `state=`; `gsp_pool_backends` now has
  `state=draining|disabled` too.
- **Phase 5 slice 2 done**: graceful connection draining. `gsp_core::drain`
  (`ConnTracker` + `ConnGuard`, a `watch<usize>` counter). Every TCP per-conn
  task and every UDP session holds a `ConnGuard`. `Runtime::shutdown_with_grace`
  signals the accept/recv loops, awaits them, then `ConnTracker::wait_idle`,
  the whole thing bounded by `settings.shutdown_grace_sec` (default 30, →
  `Config::shutdown_grace`); leftover tasks are aborted after the deadline.
  `main.rs` passes `cfg.shutdown_grace`. UDP listeners now enter a `draining`
  state on the signal — established sessions keep pumping until they idle out,
  new datagrams are dropped (`reason="draining"`). `RuntimeHandle::active_conns()`
  exposes the live count.
- **Phase 5 slice 3 done**: `RuntimeHandle::{is_draining,set_draining}` (an
  `AtomicBool`); `ready()` now also fails while draining. `gsp/src/admin.rs`:
  `POST /admin/drain` / `POST /admin/undrain` (flip `readyz`, data path keeps
  running), `GET /config` (plaintext snapshot: listeners + pools + `draining` /
  `active_conns`). `readyz` returns `draining` (503) when drained.
  `shutdown_with_grace` also sets the flag.
- **Phase 5 slice 4 done**: runtime backend CRUD. `gsp_core::overlay::BackendOverlay`
  (a `Mutex<HashMap<pool, {added,removed}>>`) layered on the file config;
  `Snapshot::build_with_overlay` runs each pool's file `targets` through it.
  `Runtime`/`RuntimeHandle` hold the overlay + a `reload_requested` `Notify`;
  `gsp/src/admin.rs` `POST /pools/{p}/backends {addr}` / `DELETE .../{addr}`
  mutate the overlay then `request_reload()`. `reload.rs` selects on that Notify
  alongside SIGHUP / file-watch and builds via `build_with_overlay`, so admin
  edits survive a file reload and there is still exactly one snapshot writer.
  `GET /config` shows `overlay=+N/-M` per pool.
- **Phase 5 slice 5 done** (closes phase 5): runtime listener add/remove/rebind.
  `gsp_core::listeners::ListenerManager` owns one task `Group` per listener
  (its `workers` accept tasks + a private `watch<bool>` stop). `Runtime::start`
  calls `start_all`; `reload::apply` calls `handle.reconcile_listeners().await`
  when `cfg.listeners` changed — diff by name, unchanged kept, changed
  stopped+respawned, added spawned, removed stopped. New groups spawn before old
  ones are awaited, so a same-bind rebind is gapless (`SO_REUSEPORT`).
  `shutdown_with_grace` now does `listeners.stop_all()` + `abort_all()`.
- **Phase 6 slice 1 done**: TCP PROXY protocol. `gsp-config`
  `ProxyProtocol { None, V1, V2 }` + `pools[].proxy_protocol` (default `none`) →
  `PoolConfig::proxy_protocol` → `Pool::proxy_protocol`. `gsp_core::proxy_protocol`
  encodes v1 (text) / v2 (binary) headers from `(client_peer, client_local)`.
  `proxy::handle_tcp` gained a `client_local: SocketAddr` param and writes the
  header to the backend right after connect, before the pump; failure ⇒ passive
  unhealthy + error. `gsp_proxy_protocol_headers_total{pool,version}`. `listener.rs`
  passes `local`. Live across reload (pool rebuild). `target` connections
  (resolver) have no pool ⇒ no header.
- **Phase 6 slices 3–4 done — phase 6 complete**: TPROXY transparent mode, TCP
  and UDP. `gsp-config` `listeners[].transparent: bool` (default false) →
  `ListenerConfig::transparent`; `validate()` rejects `transparent` + `prefix`
  together. `net.rs`: `bind_reuseport_tcp(.., transparent)` and a new
  `UdpMode { Plain, Prefix, Transparent }` for `bind_reuseport_udp`;
  `bind_transparent_udp(addr)` binds an `IP_TRANSPARENT` UDP socket to a
  non-local address (upstream source + reply socket); `connect_tcp_from(backend,
  Option<source>)` (`None` / family mismatch ⇒ plain connect; `Some` ⇒
  `TcpSocket` + `IP_TRANSPARENT` + `bind` + connect); `set_ip_transparent<F:
  AsFd>(&F, v6)` via `socket2::SockRef` (v4 **and** v6), `#[cfg(linux)]`, still
  zero `unsafe`. `socket2` bumped 0.5 → 0.6 (already in the tree via
  `hyper-util`; `set_freebind` → `set_freebind_v4/_v6`). TCP: `proxy::{connect_backend,
  handle_tcp, handle_tcp_target}` take `transparent_source: Option<SocketAddr>`;
  `listener.rs` passes `cfg.transparent.then_some(peer)` (pool **and** resolver
  `target`). UDP (`listener_udp.rs`): `recv_one`/`recvmsg_dst` read
  `IP_ORIGDSTADDR` (full `ip:port`) in transparent mode, `IP_PKTINFO` (dst IP +
  listener port) in prefix mode; the session `dst` type went `Option<IpAddr>` →
  `Option<SocketAddr>` (SessionKey, StickyKey, `local`); `connect_upstream` binds
  the client addr; a per-session reply socket (`Session::_reply_sock`) bound to
  the orig dst sends replies with a plain `send_to` (prefix mode keeps its
  `sendmsg` pktinfo path). New drop reason `reply_bind`. `GET /config` shows
  `freebind` / `transparent`. `ListenerConfig` is `PartialEq`, so flipping
  `transparent` rebinds the listener on reload.
- **Phase 6 slice 2 done**: `ProxyProtocol::V2Udp` (`proxy_protocol: v2-udp`,
  serde `rename = "v2-udp"`). `proxy_protocol::header` switches on the mode
  (`V2` → STREAM byte `0x11`, `V2Udp` → DGRAM byte `0x12`); the `stream` param is
  gone. `validate()` cross-checks each non-`none` pool against the transports of
  the listeners that statically route to it (via `Action::Pool`): v1/v2 reject a
  UDP listener, v2-udp rejects a TCP listener, both-transports rejects. Resolver
  `target` / resolver-chosen pools are not checked (runtime falls back to no
  header on a mismatch — TCP path guards on `V1|V2`, UDP path on `V2Udp`).
  `listener_udp::open_session` threads `pool.proxy_protocol` + the pool name out
  of the route match and, for `V2Udp`, prepends `header(V2Udp, client, local)`
  to the first datagram only (one `Cow::Owned` alloc; later datagrams untouched).
  `local` is the real per-datagram dest in prefix mode. `ProxyProtocol::label()`
  feeds `gsp_proxy_protocol_headers_total{pool,version}` (`v1|v2|v2-udp`).
- **Phase 7 slice 1 done**: CIDR allow/deny filter chain. `gsp-config`
  `RawListener` gains `allow: Vec<String>` / `deny: Vec<String>` → parsed to
  `Acl { allow: Vec<Cidr>, deny: Vec<Cidr> }` (`gsp_config::Acl`, `permits(ip)` /
  `is_empty()`), a new `ListenerConfig::acl` field (always present, empty =
  admit-all). Semantics: `deny` checked first and wins; a non-empty `allow` makes
  the listener default-deny. `validate()` parses each CIDR (bad string ⇒
  `Invalid`). `listener.rs` checks `cfg.acl.permits(peer.ip())` right after
  `accept()` (before the `accepted` counter / task spawn); `listener_udp.rs`
  checks `cfg.acl.permits(client.ip())` after the established-session fast path,
  before `open_session` — established UDP sessions keep a scan-free steady path.
  Blocked ⇒ silent drop (no reflection) + `gsp_filter_blocked_total{listener,
  filter="acl"}` (`m::FILTER_BLOCKED`). `GET /config` shows `acl=+N/-M`.
  `ListenerConfig` is `PartialEq` so an `allow`/`deny` change rebinds the
  listener on reload. Linear `Cidr` scan (no LPM trie yet).
- **Phase 7 slice 2 done**: per-listener rate limiting. `gsp-config`
  `RawListener::rate_limit` → `Option<RateLimit { per_ip, per_net: Option<TokenBucket
  { rate: u32, burst: u32 }> }>` (`ListenerConfig::rate_limit`); `burst` defaults
  to `rate`, `validate()` rejects `rate == 0` and an empty `rate_limit` (needs
  ≥1 bucket). `gsp-core::ratelimit::RateLimiter` — `Mutex<State { ips:
  HashMap<IpAddr,Bucket>, nets: HashMap<NetKey,Bucket> }>`, `Bucket { tokens: f64,
  last_ms }`, monotonic refill via `util::now_ms`; `NetKey` = /24 (v4) or /64
  (v6). `permit(ip) -> Option<&'static str>` refills both configured buckets,
  admits only if *both* have ≥1 token (consumes 1 from each), else returns
  `Some("rate_ip"|"rate_net")` and consumes nothing; lazy prune of full buckets
  past `PRUNE_AT = 100_000`. One `Arc<RateLimiter>` per listener built in
  `ListenerManager::spawn_group`, cloned into every worker, rebuilt on respawn
  (so a `rate_limit` edit takes effect on reload — `ListenerConfig` stays
  `PartialEq, Eq`, `TokenBucket` is integer-valued). `run_tcp_listener` /
  `run_udp_listener` gained a `limiter` param (now `#[allow(clippy::
  too_many_arguments)]` with a justification — single caller). Checked right
  after the ACL: TCP before the task spawn, UDP after the established-session
  fast path. `m::FILTER_BLOCKED` gains `filter="rate_ip"|"rate_net"`. `GET
  /config` shows `rate_limit=ip:R/B,net:R/B`.
- **Phase 7 slice 3 done**: process-wide global caps. `gsp-config`
  `settings.limits` → `Config::limits: GlobalLimits { max_connections,
  max_udp_sessions, max_new_sessions_per_sec: Option<...> }` (also copied onto
  `Snapshot::limits`); `validate()` rejects a `0` for any of the three.
  `gsp-core::limits::GlobalLimits` — two `AtomicUsize` live counters (TCP conns /
  UDP sessions) + an optional `Mutex<NewRate>` token bucket (burst = the rate)
  for new conns **and** sessions combined; `acquire_tcp()` / `acquire_udp()` →
  `Result<LimitGuard, &'static str>` check the count cap (reversible
  `fetch_add`/`fetch_sub`) then the rate bucket, consuming nothing on refusal;
  `LimitGuard` RAII-releases the count slot on drop. Built once in
  `Runtime::start` from `initial.limits`, threaded `ListenerManager` → workers →
  `run_tcp_listener` / `run_udp_listener` (`limits: Arc<GlobalLimits>` param).
  Checked right after the per-listener rate limiter: TCP holds the guard in the
  per-conn task (`_limit_guard`), UDP stores it on `Session` (drops on
  eviction). Startup-only (reload does **not** re-read — like `workers`).
  `m::FILTER_BLOCKED` gains `filter="max_conn"|"max_udp"|"max_new_rate"`. `GET
  /config` shows `limits=conn:N,udp:N,new_rate:N`.
- **Next**: phase 7 slice 4+ — UDP first-packet gate, amplifier checklist tests,
  optional geo filter, LPM trie for the ACL, fuzzing the peek/sniffer parsers.
  `proxy_protocol` on a resolver `target` (pool-less TCP) is still unaddressed.
  Deferred: `GET /sessions` (per-session registry); resolver `sticky_key`; the
  sniffer plugin loader (Phase 9).
- **Build/verify**: `make check` (fmt + clippy `-D warnings` + ~129 tests). Needs
  `protoc` on `PATH` (gRPC codegen in `crates/gsp/build.rs`).
- **Infra**: git repo, remote `github.com/Wueschli/gameserver-proxy`, branch `main`.
  Local is **ahead of `origin/main` and unpushed** — pushing is blocked in this
  environment (no credentials; the HTTPS credential helper points at a nonexistent
  Windows path). Push from a machine with credentials, or switch the remote to SSH.

---

## What works today

Run `cargo run -p gsp -- --config config.example.yaml` and you get:

- **TCP listeners**, one accept task per CPU core per listener, each on its own
  `SO_REUSEPORT` socket.
- **UDP listeners**, one recv task per CPU core per listener, each on its own
  `SO_REUSEPORT` datagram socket. Per-worker **lock-free session table** keyed by
  `(client, dst)` (dst = `None` unless prefix mode); one upstream socket
  `connect(2)`-ed to the chosen backend per session plus a reply-pump task;
  `src_ip` / `src_ip_port` **backend affinity** via a per-worker sticky table;
  **idle-timeout eviction** (1 s sweep, `idle_timeout_sec` from the pool, read
  once at session creation) that releases the `BackendGuard`; **amplification
  guard** — the proxy never sends to a client without an established
  session.
- **Per-listener route rule list** (`listeners[].routes`, priority-ordered, first
  match wins; `action: { pool }`). Matchers: `always`; `client_cidr` (source IP,
  hand-rolled CIDR in `gsp-config` — no `ipnet` dep); `dst` (destination IP —
  `ctx.local.ip()`: `getsockname` on TCP, the bind addr on a plain UDP listener,
  the real per-datagram destination in prefix mode — vs the same `Cidr` list);
  `port` (destination port from the accepting socket, single or `"lo-hi"`
  range); `first_bytes` (a
  `prefix`, `hex:` / `ascii:`, ≤ `FIRST_BYTES_PREFIX_MAX` = 512 B, **and/or** a
  `length: { min, max }` byte-count window — `Matcher::FirstBytes { prefix, len
  }`, at least one present; on TCP `length` sees only what one peek returned);
  `sni` (host from the peeked TLS ClientHello — `gsp_config::extract_sni`, a
  hand-rolled ClientHello reader; `host` patterns exact / `*.suffix` / `.suffix`;
  rejected on UDP listeners); `sniffer` (a named plugin resolved via
  `gsp_core::sniff::sniffer(name)` — **currently always `None`; no built-ins**;
  when present it runs once per conn on the peeked bytes and its `RouteHint`
  goes into `MatchContext.sniff`; optional `host` patterns match the hint's
  host, empty ⇒ match on any non-`reject` recognition; **one sniffer name per
  listener**, enforced in `validate()` → `ListenerConfig::sniffer`; an unknown
  name logs a warning at listener start and its routes never match). A bare
  `pool:` is normalised to one `always` route. **Push resolver**: a listener
  with `route_hint: true` first checks `RouteHints::lookup(src.ip())` (fed by
  `POST /route-hint`) and a live hint whose pool still exists wins over the
  route list (`gsp_route_hints_applied_total{listener}`). No match →
  connection/datagram dropped
  (`gsp_listener_connections_total{result="no_route"}` /
  `gsp_datagrams_dropped_total{reason="no_route"}`). TCP `MSG_PEEK`s
  `ListenerConfig::peek_len()` bytes — up to `PEEK_MAX` = 4096, i.e. an `sni`
  route peeks the full 4096 — (250 ms budget, `PEEK_TIMEOUT`) in the spawned
  per-conn task, only when a route needs bytes; UDP routes on the first datagram
  it already holds. A silent TCP client (or a ClientHello fragmented past the
  first segment) routes as if it sent nothing.
- **UDP prefix mode** (`listeners[].prefix: <cidr>`, wildcard `bind` required):
  one socket carries `IP_PKTINFO` / `IPV6_RECVPKTINFO`; the recv path uses
  `recvmsg` to read the datagram's real destination, feeds it to routing
  (`dst`), keys the session by `(client, dst)`, and the reply pump sends with
  `sendmsg` + a pktinfo cmsg so the client sees the reply from the address it
  hit. Destination outside the prefix →
  `gsp_datagrams_dropped_total{reason="outside_prefix"}`. Implemented with `nix`
  (safe wrappers — no `unsafe`; ADR 10). **TCP `freebind: true`** sets
  `IP_FREEBIND` / `IPV6_FREEBIND` on the bind socket.
- **Balancers**: `round_robin`, `least_conn` (counts UDP sessions too),
  `consistent_hash` (rendezvous/HRW hash of the client key — pool `hash_on:
  src_ip | src_ip_port` — over the healthy backends; `acquire()` with no client
  key falls back to round-robin). TCP passes `peer`; UDP passes the client addr
  on the non-sticky path.
- **Per-connection pump**: buffered bidirectional copy, connect timeout, per-direction
  idle timeout, half-close propagation, `TCP_NODELAY`.
- **Client-IP preservation**: a pool with `proxy_protocol: v1 | v2` (TCP) gets one
  PROXY protocol header written to the backend before any client bytes, carrying
  the real `(client, proxy-local)` addresses; `v2-udp` prepends the v2 binary
  header to the first datagram of each UDP session (later datagrams untouched).
  `none` by default; the form is validated against the listener transport.
  Resolver `target` connections get no header (no pool). Alternatively, a TCP or
  UDP listener with `transparent: true` (Linux TPROXY) makes every upstream
  connection / datagram source from the real client `ip:port`, and (UDP) replies
  come from the original destination address — no protocol change, needs
  `CAP_NET_ADMIN` + return routing through this host; mutually exclusive with
  `prefix`.
- **Backend health**: active `tcp_connect` or `udp_probe` probes per pool
  (`udp_probe` sends `send_hex`, expects a reply datagram optionally prefix-matched
  by `expect_hex_prefix`); `interval`, `timeout`, `rise`, `fall`; passive marking on
  connect failure/timeout (TCP) and on upstream-socket failure (UDP); unhealthy
  backends are excluded from selection; `PickError` when none are available.
- **Per-backend session cap** (`per_backend.max_sessions`) — shared by TCP
  connections and UDP sessions.
- **Hot reload**: `SIGHUP` or config-file change → validate → rebuild `Snapshot` →
  atomic `ArcSwap` store. Invalid config is rejected and the running config kept.
  Backend health is carried across the swap by address. Pool membership / balancer /
  health-check / cap changes are **live**. **Listeners are reconciled by name**
  (`ListenerManager::reconcile`): an added listener is spawned, a removed one is
  stopped, and one whose `ListenerConfig` changed (bind, protocol, routes,
  affinity, `prefix`, `freebind`, `route_hint`) is stopped and re-spawned — a
  same-bind rebind is gapless (`SO_REUSEPORT`, new sockets bind first). An
  unchanged listener keeps running, its route rules captured at spawn; only the
  *pool contents* they resolve to are read live. Route-**hint entries** and
  **backend overlay** edits are runtime state, independent of the file.
- **Admin API** (`settings.admin.listen`, default `127.0.0.1:9900`):
  `GET /healthz` `/readyz` `/metrics` (Prometheus) `/pools` `/config`;
  `POST /route-hint` `{src_ip, pool, ttl_sec}` (push resolver — validates the
  pool, `ttl_sec` 1..=3600); `PATCH /pools/{pool}/backends/{addr}` `{state:
  enabled|draining|disabled}` (operator backend state — `draining`/`disabled`
  divert new sessions, existing ones drain; carried across reload);
  `POST /pools/{pool}/backends {addr}` / `DELETE /pools/{pool}/backends/{addr}`
  (add/remove a backend at runtime — overlay-backed, survives a file reload);
  `POST /admin/drain` + `POST /admin/undrain` (flip `readyz` for the LB without
  stopping the data path). `GET /config` is a plaintext snapshot dump with
  `draining` / `active_conns` and `overlay=+N/-M` per pool. `gsp` now depends on
  `serde` for the request bodies.
- **Metrics**: see `crates/gsp-core/src/metrics_defs.rs`. Connections, bytes,
  duration, backend connect errors, `gsp_pool_backends`, `gsp_healthcheck_total`,
  `gsp_lb_selections_total`, `gsp_route_hints_applied_total{listener}`,
  `gsp_config_reload_total`, `gsp_config_version`, and for UDP:
  `gsp_active_udp_sessions{listener}`, `gsp_packets_total{listener,dir}`,
  `gsp_datagrams_dropped_total{listener,reason}`.
- **Graceful stop** on SIGINT/SIGTERM: the accept / recv loops and the health
  checker stop taking new work; in-flight TCP connections and established UDP
  sessions keep running and are waited on (`ConnTracker`), bounded by
  `settings.shutdown_grace_sec` (default 30). UDP sessions drain until they idle
  out; new datagrams to a draining listener are dropped
  (`gsp_datagrams_dropped_total{reason="draining"}`).

### Tests (75, all green)

- `gsp-config` (38): schema parsing + validation rejections, incl. UDP listener +
  default affinity, affinity-on-TCP rejection, `udp_probe` parsing, `udp_probe`
  without `send_hex` rejection, `consistent_hash` parsing + default/explicit
  `hash_on`, `hash_on`-without-`consistent_hash` rejection; **routing**: bare
  `pool` → one `always` route, route-list first-match (`client_cidr` / `port` /
  `always`), `pool`+`routes` rejection, unknown-pool-in-route rejection,
  `always`-with-fields rejection, bad-CIDR / reversed-range / empty-`cidrs`
  rejection, `Cidr::contains` v4 + v6, `dst` select-by-destination-IP (v4 + v6)
  + `dst`-without-`cidrs` / `cidrs`-on-wrong-type rejections,
  `first_bytes` prefix match + `peek_len()`,
  `first_bytes` `length` bound + `prefix`+`length` combined + `peek_len` from the
  bound, bad `first_bytes` specs (incl. `min > max`, `length` on a non-first_bytes
  matcher); `extract_sni` from a crafted ClientHello (+ truncated
  / non-handshake → `None`), `sni` exact + `*.suffix` matching (`*.foo` ≠ apex),
  bad `sni` config (on UDP, empty `host`, `a*b`, wrong field); UDP `prefix`
  listener parse + `Cidr::contains`, TCP `freebind` parse, and rejections
  (`prefix` on TCP / with a non-wildcard bind / unparseable, `freebind` on UDP);
  `sniffer` matcher parse + `ListenerConfig::sniffer`, `Matcher::Sniffer` match
  (exact / suffix / empty-host / `reject` / no-hint), bad `sniffer` config
  (missing name, wrong field, two sniffers on one listener); `route_hint: true`
  listener flag parse; **resolver**: `resolvers:` + `Action::Resolver` parse +
  resolver route forces `peek_len == PEEK_MAX`, bad-resolver / bad-action
  rejections (unknown resolver, both `pool`+`resolver`, neither, unknown
  transport, empty endpoint); `cache:` parse (`key` parts incl.
  `first_bytes:a:b`, TTLs, `max_entries`) + bad-cache rejections (empty key,
  unknown part, `a>b`, `b>PEEK_MAX`, `max_entries: 0`).
- `gsp-core` unit (23): round-robin cycling, least-conn preference, capacity
  rejection, unhealthy-skip, all-unhealthy error, `rise`/`fall` thresholds,
  reload health carry-over; `consistent_hash` stability + spread (`src_ip`
  ignores port), and "only the lost backend's share moves"; **sniff seam**:
  registry has no built-ins but knows the `#[cfg(test)]` `test-host` sniffer,
  `test-host` extraction, end-to-end `sniffer`-matcher routing driven by that
  test sniffer; **route hints**: `RouteHints` set / lookup / replace / expiry +
  prune-on-write; **resolver** (`resolver.rs`): `resolve_route` with a stub —
  `pool` / `target` results used, `empty`/`error` under `reject` → drop, under
  `fallback_route` → next matching route; two end-to-end live connections
  through `Runtime` (one routed to a pool, one straight to a `target` address in
  no pool); **cache** (`CachedResolver` + a call-counting stub): repeats served
  from cache & keyed by `src_ip`, negative caching absorbs retries, `stale_ok`
  serves an expired positive, an uncacheable request (missing SNI key part)
  always calls through.
- `gsp` unit (3): local base64 encoder known vectors; **gRPC** round trip — an
  in-process `tonic` `Resolver` server echoes the request SNI into the pool
  name, `GrpcResolver::new` + `resolve` against it.
- `gsp-core/tests/tcp_forward.rs` (6): end-to-end client→proxy→backend byte
  forwarding; "routes around a dead backend"; "first matching route selects the
  pool" (`client_cidr` hit vs. fall-through to `always`); "consistent_hash pins
  a client to one backend"; "sni matcher routes by ClientHello"; "route_hint
  overrides the route list" (push a hint via `RuntimeHandle::route_hints`, and
  an unknown-pool hint is ignored).
- `gsp-core/tests/udp_forward.rs` (5): end-to-end UDP datagram forwarding + session
  reuse / affinity (same client → same backend); idle-timeout eviction frees the
  per-backend slot; `first_bytes` prefix routes to its pool vs. `always`;
  `first_bytes` `length` routes short vs. long datagrams; **prefix listener**
  routes `127.0.0.2` vs `127.0.0.3` (real `IP_PKTINFO` recv) and the client —
  `connect`-ed to the sub-address — only accepts the reply if its source is that
  address, proving the `sendmsg` pktinfo path.

---

## Decisions already locked

From `docs/09-technology-choices.md` (ADR table) and implementation:

| # | Decision |
|---|----------|
| Lang | **Rust**, edition 2021, toolchain pinned `stable` (`rust-toolchain.toml`). |
| Runtime | **`tokio`** multi-thread. io_uring (`monoio`/`glommio`) is a later optimization behind an IO abstraction — not now. |
| Config | Immutable `Snapshot` behind `arc_swap::ArcSwap`. `serde_yaml` (deprecated but working; revisit if it breaks). |
| Data/control split | Data plane only reads the snapshot; `reload.rs` is the only writer. |
| LB / health | `AtomicBool` healthy flag, `rise`/`fall` streaks under a short `Mutex`, `AtomicUsize` active count. `BackendGuard` RAII for the session slot + passive health. |
| Balancers | `round_robin` (atomic index + `rotate_left`), `least_conn` (sort healthy by active), `consistent_hash` (rendezvous/HRW hash via `std` `DefaultHasher`; no `hashring` dep — backend set is tiny). |
| UDP | Worker-local session table (no global lock), `connect(2)` socket + reply task per session, per-worker sticky affinity table (hard cap, wholesale clear), 1 s idle sweep. `recvmmsg`/`sendmmsg`, timing wheel deferred. See ADR 9. `consistent_hash` now gives table-free affinity as an alternative to the sticky table. |
| UDP prefix routing | One wildcard `IP_PKTINFO` socket per prefix (`recvmsg` for the real dest, `sendmsg` cmsg for the reply source), via `nix` — zero `unsafe`. See ADR 10. |
| Discovery adapters, sniffers | **designed in `docs/`, not yet built.** |
| PROXY protocol (`proxy_protocol: v1 / v2 / v2-udp`) + TPROXY transparent mode (`transparent: true`, TCP + UDP) | **done** (phase 6). `set_ip_transparent` via `socket2` 0.6 `SockRef`; origdst via `nix` — still zero `unsafe`. |
| External resolver | `trait Resolver` + cache + `on_error` + routing loop in `gsp-core`; HTTP/gRPC clients in the `gsp` binary, injected as `Arc<dyn Resolver>` (same pattern as the sniffer seam). Keeps HTTP out of `gsp-core`. |
| Deps kept out of `gsp-core` | `axum`, `clap`, `notify`, `reqwest` live in the `gsp` binary only. (`gsp-core` uses `nix` for `IP_PKTINFO` / `IP_ORIGDSTADDR` cmsgs, `socket2` 0.6 for `IP_TRANSPARENT` / `IP_FREEBIND`, and `async-trait` for `Resolver`.) |

---

## Known limitations / deferred (with the phase that addresses them)

| Item | Deferred to |
|------|-------------|
| `splice()` zero-copy TCP fast path (buffered copy for now, behind the same fn) | perf pass, any time |
| UDP `recvmmsg`/`sendmmsg` batching (plain `recv_from`/`send` now) | perf pass |
| UDP idle expiry via a timing wheel (1 s sweep now) | perf pass |
| UDP sticky-affinity table: LRU eviction (hard cap + wholesale clear now) | polish |
| `consistent_hash` balancer | **done** (phase 3 slice 3) |
| `consistent_hash` used to retire the UDP per-worker sticky table | polish |
| `weighted` / `first_available` balancers | later |
| UDP ICMP port-unreachable as an explicit passive health signal (currently just ends the reply pump; the idle sweep reaps) | phase 5–7 |
| Listener add / remove / rebind at runtime | **done** (phase 5 slice 5) |
| Tracked connection drain with a grace period on shutdown | **done** (phase 5 slice 2) |
| CRUD admin API: `POST` / `DELETE` a backend, `GET /config`, `POST /admin/drain`, `PATCH` backend state | **done** (phase 5 slices 1, 3, 4) |
| `GET /sessions` introspection (needs a per-session registry) | phase 5 / later |
| `draining` / `disabled` backend states | **done** (phase 5 slice 1) |
| Reload debounce only coalesces within one 200 ms window; wider-spaced events cause separate (idempotent) reloads | polish, low priority |
| **Phase 3 — done** (slices 1–9): route rule list; matchers `always` / `client_cidr` / `dst` / `port` / `first_bytes` (`prefix`+`length`) / `sni`; `consistent_hash` balancer; UDP `prefix:` listener + TCP `freebind:`; sniffer API seam + `sniffer` matcher (no built-ins); `POST /route-hint` push resolver | **done** |
| Sniffer plugin **loader** + a generic `first_bytes` `regex` matcher (as a plugin) — separate community repo, sandboxed/WASM, runtime-loaded | **Phase 9** |
| `first_bytes` `regex` variant (needs `regex` dep; goes in the `gsp_core::sniff` layer, not `gsp-config`) | phase 3 |
| More sniffers (`quic`, `wireguard`, …); `RouteHint.reject` currently only makes a `sniffer` route *not match* (no hard drop) | phase 3+ |
| Per-listener multiple distinct sniffers (only one name allowed today) | polish |
| TCP prefix binding beyond `freebind` (accepting a whole prefix on one socket — needs routing + `getsockname`, no cmsg), IPv4 non-local bind ergonomics | phase 3–6 |
| **Phase 4 — done**: external resolver HTTP + gRPC, `pool` + `target`, `on_error`, TTL LRU cache + `stale_ok` | **done** |
| **Phase 6 — done**: PROXY protocol v1/v2 (TCP) + v2-udp (first datagram); TPROXY transparent mode (TCP + UDP, v4 + v6) | **done** |
| UDP transparent `IPV6_TRANSPARENT` on a musl / non-glibc target, and `recvmmsg` batching for the transparent recv path | perf / portability pass |
| Resolver `sticky_key` — resolver-chosen affinity key; deferred (overlaps the request-keyed cache + `route_hint` + UDP affinity; needs a design for how a later request recovers the key) | later |
| `target` connections use fixed 300 ms connect / 90 s idle timeouts (`proxy::TARGET_*`) — no pool to read them from; a per-resolver knob could come later | polish |
| Build now needs `protoc` (gRPC codegen in `crates/gsp/build.rs`); CI installs `protobuf-compiler` | — |
| Resolver cache uses `std::sync::Mutex<LruCache>` — a brief lock on the routing path (not held across `.await`); like `Backend::observe`, deliberate | — |
| Resolver config is startup-only (no live reload of `resolvers:`); a resolver call is a per-connection `.await` bounded by `timeout_ms` | — |
| `route_hint` per-conn cost adds a lock-free `ArcSwap<HashMap>` read when the listener opts in — recorded in the latency ledger | — |
| `sni` on a ClientHello split across TCP segments (single peek only; falls through) | polish |
| Backend discovery adapters (DNS SRV, K8s, Consul) | phase 8 |
| CIDR allow/deny filter chain (per-listener `allow` / `deny`) | **done** (phase 7 slice 1) |
| Rate limiting (per-listener token bucket, src_ip + /24 / /64) | **done** (phase 7 slice 2) |
| Global caps (`max_connections` / `max_udp_sessions` / `max_new_sessions_per_sec`) | **done** (phase 7 slice 3) |
| Geo filter, UDP first-packet gate, ACL LPM trie | phase 7 |
| `panic = "abort"` in the release profile — fine, but be aware unwinding is off | — |

---

## Latency ledger

Per-TCP-connection cost: 1 `Pool::acquire` (lock-free reads + one atomic add),
1 backend `TcpStream::connect`, 1 spawned task for the pump. No per-byte allocation
beyond the two 32 KB direction buffers. Nothing per-connection touches a lock.

Per-UDP-session cost (paid once, on the first datagram of a session): 1
`Pool::acquire` / `acquire_addr`, 1 `UdpSocket::bind` + `connect` for the upstream
socket, 1 spawned reply-pump task, 1 sticky-table insert, 1 session-table insert.
Two 64 KB buffers per session (one in the recv loop, shared across all of that
worker's sessions; one per reply task). **Per-datagram** cost on the steady-state
path: one `HashMap` lookup by client `SocketAddr`, one relaxed atomic store
(liveness), one `send`/`send_to` — no lock, no allocation, no task spawn.

Phase 3 routing adds, per new TCP connection / new UDP session only: one
`stream.local_addr()` (TCP) or cached `down.local_addr()` (UDP) syscall and a
linear scan of the (small, fixed) route list — bit-compare per `client_cidr`
entry (and per `dst` entry against `ctx.local.ip()`), `u16` range check per
`port` entry, `starts_with` + a `len()` range check
per `first_bytes` entry, and for an `sni` entry one pass of `extract_sni` over
the peek buffer (bounded walk of the ClientHello, no alloc except the returned
host `String`).
When (and only when) a route uses `first_bytes` / `sni` / `sniffer`, the TCP
path also does one `MSG_PEEK` (into a `peek_len()`-sized `Vec` — `PEEK_MAX` =
4096 for `sni` / `sniffer`) with a 250 ms timeout, and clones the listener's
`Arc<ListenerConfig>` into the per-conn task. A `sniffer` route would also run
the plugin once over the peeked bytes — but there are no plugins today, so
`sniffer(name)` returns `None` and that cost is currently zero; the Phase 9
loader must keep the parse bounded (time + memory) since it is on the
per-connection path. No lock, no task spawn beyond the existing per-conn one,
nothing on the per-byte / per-datagram path. TCP route resolution now happens
inside the spawned task, so the accept loop no longer loads the snapshot.

`consistent_hash` selection is `O(healthy)` — one `Vec<&Backend>` of the healthy
set (already built for every balancer) plus a `sort_by_key` with one
`DefaultHasher` (SipHash of client IP [+ port] and backend addr) per backend. No
allocation beyond that `Vec`, no lock. `least_conn` already sorts the same `Vec`,
so this is the same order of work.

**UDP prefix mode** changes the per-datagram receive from `recv_from` to
`readable().await` + `try_io(recvmsg)` (one extra `recvmsg` with a small
`cmsg_space!(in6_pktinfo)` stack buffer, walked once for the dest address) and
the per-reply send from `send_to` to `writable().await` + `try_io(sendmsg)` with
a one-element pktinfo cmsg. Still no lock, no heap alloc on the steady path
(`recvmsg`'s cmsg buffer is a fixed-size array). Non-prefix listeners keep the
exact `recv_from` / `send_to` path.

**`route_hint`** adds, per new connection / new UDP session on a listener with
`route_hint: true` only: one `ArcSwap::load` + `HashMap::get` on the hint table
(lock-free read), plus one `String` clone when a hint is present. The table is
tiny and rarely written (one admin `POST` per session). Listeners without the
flag pay nothing.

**External resolver**: a route with a matching `resolver` action does one
`.await`-ed HTTP/gRPC round-trip (`timeout_ms`, default 40 ms) on the
per-connection routing path, plus a `Vec<Action>` of the matching routes and a
`first.to_vec()` for the request body. It runs in the spawned per-conn task
(TCP) / `open_session` (UDP) — never on the accept loop. With `cache:` set a
repeat key is a `Mutex<LruCache>` get instead. Listeners with no `resolver`
route pay nothing (the loop is just `matching_routes` → `Pool`). A `target`
connection skips the `Pool::acquire_for` (no LB sort, no atomic, no
`BackendGuard`) — strictly cheaper than a pooled one.

**Listener reconcile** (`ListenerManager`): control-plane only — a reload with
changed `listeners` walks the (small) listener list and spawns/stops task
groups. Zero data-path cost; running accept loops are untouched when their
config is unchanged.

**Backend overlay** (`POST`/`DELETE` backend): touched only during a snapshot
rebuild (`effective_targets`, one `Mutex` lock + a small `Vec` per pool). Zero
data-path cost.

**Connection draining** (`ConnTracker`): one `watch::Sender::send_modify` (a
brief internal lock, no `.await`) on connection/session open and again on close —
once per TCP connection and once per UDP session, never per byte or per datagram.
Nothing on the steady-state path.

**Transparent mode** (`transparent: true`): per new TCP connection / UDP session
only — the upstream socket gains one `setsockopt(IP_TRANSPARENT)` + one `bind`
before connect, and a UDP session also binds one extra `IP_TRANSPARENT` reply
socket. UDP transparent recv uses `recvmsg` + a fixed-size cmsg buffer instead of
`recv_from` (same as prefix mode). A couple of extra syscalls at session setup;
no allocation, no lock, nothing per byte / per steady-state datagram.
Non-transparent listeners keep the plain `TcpStream::connect` / `recv_from`
paths.

**PROXY protocol** (`proxy_protocol: v1 | v2`): one `Vec` allocation (≤ 52 B) and
one extra `write_all` to the backend per new TCP connection, before the pump.
`v2-udp`: on the **first** datagram of a UDP session only, one `Cow::Owned`
(header + payload, ≤ 28 B + datagram) and it replaces the plain `send(first)` —
no extra syscall. Steady-state datagrams are byte-for-byte unchanged. Only when
the pool opts in; `none` pools and resolver `target` connections pay nothing. No
lock, no task, nothing per byte.

**Filter chain — CIDR allow/deny** (`allow` / `deny` on a listener): per new TCP
connection / new UDP session only, one linear scan of the (small, fixed) `Cidr`
list — bit-compare per entry, no alloc, no lock, no task. TCP runs it before the
task spawn; UDP runs it only for datagrams that don't hit an established session,
so the steady-state per-datagram path is unchanged. Listeners with neither list
pay nothing (`Acl::is_empty`, but the scan over two empty `Vec`s is already ~free).

**Rate limiting** (`rate_limit` on a listener): per new TCP connection / new UDP
session only — one `Mutex<HashMap>` lock (not held across `.await`, like
`Backend::observe`), one or two `HashMap` entry lookups, f64 refill arithmetic,
no task, no per-connection heap alloc (entries are reused; prune is amortised).
UDP runs it only for datagrams that miss an established session, so the
steady-state per-datagram path is untouched. Listeners without `rate_limit` pay a
single `is_enabled()` bool check (`permit` short-circuits before locking).

**Global caps** (`settings.limits`): per new TCP connection / new UDP session
only — one or two relaxed-ish atomic ops (`fetch_add` + maybe `fetch_sub`) and,
if `max_new_sessions_per_sec` is set, one short `Mutex<NewRate>` lock (no
`.await`). One `LimitGuard` created/dropped per connection / session, never per
byte / datagram. Uncapped ⇒ a single `is_enabled()` check.

**If you add a per-connection or per-datagram task, hop, or allocation, record it
here.**

---

## Phase 2 — UDP (done)

Landed as designed. `protocol: udp` listeners forward datagrams through the same
pool / health / cap machinery with per-client sessions. Key files:
`gsp-core/src/listener_udp.rs`, `gsp-core/src/net.rs` (`bind_reuseport_udp`),
`gsp-core/src/health.rs` (`udp_probe`), `gsp-config` (`Protocol::Udp`,
`HealthCheckKind`, `HashOn`, listener `affinity`).

First-cut simplifications, each a drop-in replacement later (see the deferred
table and ADR 9): plain `recv_from`/`send` instead of `recvmmsg`/`sendmmsg`; a 1 s
idle sweep instead of a timing wheel; the sticky-affinity table is bounded by a
hard cap and cleared wholesale (no LRU); ICMP port-unreachable just ends the reply
pump. `consistent_hash` was **not** added (still `round_robin` / `least_conn`).

Reload contract held: a reload rebuilds pools; live UDP sessions keep running
against their existing `Arc<Backend>` (the `BackendGuard` keeps the slot), and the
idle timeout is read once per session at creation.

## Phase 3 — routing intelligence

Design reference: `docs/03-routing.md` and `docs/08` phase 3.

### Slice 1 — address-based route rule list (done)

`listeners[].routes` is a priority-ordered `[{ match, action }]` list, first
match wins, `action: { pool: <name> }`. `pool:` and `routes:` are mutually
exclusive; a bare `pool:` is normalised to a single `always` route in
`validate()`. Matchers: `always`, `client_cidr` (source IP, any of N prefixes),
`port` (destination port from the accepting socket — `stream.local_addr()` on
TCP, the listener socket's local addr on UDP; single port or `"lo-hi"` range).
No match → drop with the `no_route` metric label.

Key code: `gsp-config/src/lib.rs` — `Cidr` (hand-rolled, no `ipnet`: keeps the
serde-+-thiserror-only rule), `Matcher`, `Route`, `MatchContext`,
`ListenerConfig::route_for` / `peek_len`, `parse_matcher` / `parse_port_range`.
`gsp-core/src/listener.rs` and `listener_udp.rs` build a `MatchContext` and call
`cfg.route_for(&ctx)` then `snap.pool(name)`. The UDP idle timeout is now read
from the **routed** pool per session (was a single startup read of `cfg.pool`).

### Slice 2 + 5 — `first_bytes` matcher (`prefix` + `length`) (done)

`match: { type: first_bytes, prefix: "hex:ff.." | "ascii:GET ", length: { min, max } }`
— `Matcher::FirstBytes { prefix: Vec<u8>, len: Option<RangeInclusive<usize>> }`.
At least one of `prefix` (≤ `FIRST_BYTES_PREFIX_MAX` = 512 B) / `length` must be
present; when both, both must hold. `prefix` matched with `starts_with`;
`length` against `ctx.first_bytes.len()` — on TCP that is only what one peek
returned (coarse), on UDP the exact datagram length. `MatchContext` carries
`first_bytes: &[u8]` (empty when nothing was peeked / sent).
`ListenerConfig::peek_len()` = max over the route list of (`prefix.len()`,
`length.max + 1` clamped to `PEEK_MAX`), 0 when no byte matcher — the TCP path
skips the peek entirely in that case. TCP peek: `TcpStream::peek` in the
per-conn task, `PEEK_TIMEOUT` = 250 ms; a silent client routes as if it sent
nothing. UDP: routes on the first datagram, already in hand in `open_session`.
`parse_byte_spec` handles the `hex:` / `ascii:` tags.

### Slice 3 — `consistent_hash` balancer (done)

`balancer: consistent_hash` + pool-level `hash_on: src_ip | src_ip_port`
(default `src_ip`; rejected on the other balancers, resolved to
`PoolConfig::hash_on: Option<HashOn>` / `Pool::hash_on`). Selection: rendezvous
(HRW) — `hrw_score(hash_on, client, backend)` hashes the client IP (and port for
`src_ip_port`) plus the backend addr with `std` `DefaultHasher`; the healthy set
is sorted by descending score, so capacity fall-through is deterministic and
losing a backend only moves that backend's share.

`Pool::acquire()` → `acquire_for(None)` (RR/LC unchanged; `consistent_hash` with
no key falls back to RR). `Pool::acquire_for(Some(client))` is the keyed entry:
`proxy::handle_tcp(stream, peer, pool)` passes the TCP `peer`; `listener_udp`
passes the client addr on the non-sticky fallback path (the sticky table still
runs first when the listener has `affinity`, and is redundant-but-harmless with
`consistent_hash`).

`DefaultHasher` is process-stable only — fine here: a reload rebuilds pools and
there is no cross-instance shared state. Docs/09 records the no-`hashring`
choice.

### Slice 4 — `sni` matcher (done)

`match: { type: sni, host: ["eu.example.com", "*.eu.example.com", ".eu.example.com"] }`
— TCP only (rejected in `validate()` on UDP listeners). `Matcher::Sni(Vec<HostPattern>)`;
`HostPattern::Exact` / `Suffix(".foo")` (both `*.foo` and `.foo` normalise to the
leading-dot suffix, so `*.foo` does **not** match the apex `foo`). All patterns
lowercased at parse.

`gsp_config::extract_sni(&[u8]) -> Option<String>` is a self-contained,
allocation-light TLS ClientHello reader (record header → handshake header →
ClientHello body → extensions → `server_name` `host_name`); returns `None` on
anything malformed or truncated. `Matcher::peek_len()` returns `PEEK_MAX` (4096)
for an `sni` route, so the TCP peek buffer covers a normal ClientHello. Single
`TcpStream::peek` only — a ClientHello spread across segments falls through
(noted in the deferred table).

`PEEK_MAX` was bumped 512 → 4096 (peek buffer cap); `first_bytes` prefixes keep
their own 512 B cap as `FIRST_BYTES_PREFIX_MAX`.

### Slice 6 — `dst` matcher, address form (done)

`match: { type: dst, cidrs: [...] }` → `Matcher::DstCidr(Vec<Cidr>)`, mirrors
`client_cidr` but tests `ctx.local.ip()`. Shares the `cidrs` raw field with
`client_cidr` (the `allow(...)` guard permits it for both); the `parse_matcher`
arm is `"client_cidr" | "dst"`, picking the variant by `m.kind`.

### Slice 7 — UDP `prefix:` listener + TCP `freebind:` (done)

Config: `listeners[].prefix: <cidr>` (UDP only, resolved to
`ListenerConfig::prefix: Option<Cidr>`; requires a wildcard `bind`) and
`listeners[].freebind: bool` (TCP only). Both are restart-only (part of
`ListenerConfig`).

`net.rs`: `bind_reuseport_udp(addr, pktinfo)` — with `pktinfo`, `nix`
`setsockopt(Ipv4PacketInfo | Ipv6RecvPacketInfo)`; `bind_reuseport_tcp(addr,
backlog, freebind)` — with `freebind`, `socket2` `set_freebind[_ipv6]`.

`listener_udp.rs`: `recv_one(sock, buf, pktinfo)` — plain `recv_from` when off,
else `readable().await` + `try_io(recvmsg_pktinfo)`. `recvmsg_pktinfo` builds a
`nix::cmsg_space!(in6_pktinfo)` buffer, `recvmsg::<SockaddrStorage>`, reads the
client from `msg.address` (`sockaddr_to_std`) and the dest from
`ControlMessageOwned::Ipv4PacketInfo.ipi_addr` / `Ipv6PacketInfo.ipi6_addr`
(`.to_ne_bytes()` for the v4 `s_addr`). Session key is `(SocketAddr,
Option<IpAddr>)`; `open_session` takes `dst`, sets `local = SocketAddr::new(dst,
port)` in prefix mode. Reply pump calls `send_reply(down, data, client, src)` —
`send_to` when `src` is `None`, else `writable().await` +
`try_io(sendmsg_pktinfo)` with `ControlMessage::Ipv4PacketInfo {
ipi_spec_dst=src, ipi_addr=0 }` / `Ipv6PacketInfo`. Dest outside the prefix →
drop, `gsp_datagrams_dropped_total{reason="outside_prefix"}`. All safe wrappers
— **still zero `unsafe`**. New dep: `nix` (`socket`, `net`, `uio`) in `gsp-core`.

### Slice 8 — sniffer API seam + `sniffer` matcher (done)

The seam a future loader (Phase 9) fills — **the proxy ships no game sniffers**;
embedding some games and not others is exactly the inconsistency we want to
avoid, and game-protocol code should be maintained/loaded separately, not
forked in.

`gsp_core::sniff`: `trait Sniffer { name(); sniff(&[u8]) -> Option<RouteHint> }`
+ `fn sniffer(name) -> Option<&'static dyn Sniffer>` (returns `None` for every
real name; a `#[cfg(test)]` build knows `"test-host"`) + `warn_if_missing()`
(logs at listener start when a `sniffer:` name resolves to nothing).
`RouteHint { host, key, reject }` lives in `gsp-config` (plain struct, no dep)
so `Matcher` can consult it via `MatchContext.sniff`.

`gsp-config`: `Matcher::Sniffer { name, host: Vec<HostPattern> }`; `RawMatch`
gains `sniffer`, shares `host` with `sni`. **No `KNOWN_SNIFFERS`** — once
sniffers load dynamically, `gsp-config` cannot know valid names; it only checks
the name is non-empty. `validate()` still enforces one sniffer name per listener
→ `ListenerConfig::sniffer: Option<String>`.

`gsp-core`: `listener.rs` / `listener_udp.rs` call `warn_if_missing` at start,
then per conn run
`cfg.sniffer.and_then(sniff::sniffer).and_then(|s| s.sniff(first))` and pass
`hint.as_ref()` into `MatchContext.sniff`.

Not done: `RouteHint.reject` only makes a `sniffer` route *not match* (no hard
drop); the loader itself is Phase 9.

### Slice 9 — `POST /route-hint` push resolver (done — closes phase 3)

`gsp_core::route_hint::RouteHints` — `ArcSwap<HashMap<IpAddr, {pool, expiry}>>`;
`set()` (rcu, prunes expired), `lookup()` (lock-free). Held by `Runtime` /
`RuntimeHandle` (`route_hints()`), passed into `run_tcp_listener` /
`run_udp_listener`. `gsp-config`: `listeners[].route_hint: bool`. In routing,
when `cfg.route_hint`, `hints.lookup(src.ip())` filtered by "pool still exists"
wins over `route_for` (bumps `gsp_route_hints_applied_total{listener}`).
`gsp/src/admin.rs`: `POST /route-hint` (`Json<{src_ip, pool, ttl_sec}>`;
validates the pool against the live snapshot, `ttl_sec` 1..=3600) → `gsp` now
pulls in `serde`.

**Phase 3 is complete.** Remaining routing work is deliberately elsewhere:
`external` resolver = phase 4; sniffer loader + `first_bytes` `regex` (as a
plugin) = phase 9; `weighted` / `first_available` balancers = later.

## Phase 4 — external routing logic

Plan: **slice 1** HTTP resolver + `pool` + `on_error` (done); **slice 2** the
result cache (configurable key `src_ip` / `sni` / `routing_key` /
`first_bytes:a:b`, positive/negative TTL, `max_entries` LRU, `on_error: stale_ok`);
**slice 3** gRPC transport (`tonic` + `prost` + `resolver.proto` + `build.rs`);
**slice 4** `Resolution.target` (a pool-less connect path in `proxy.rs` /
`listener_udp.rs`) + `sticky_key` (a per-listener sticky table so repeat clients
skip the resolver).

### Slice 4 — `Resolution.target` (done)

`resolve_pool` → `resolve_route` returning `Routed { Pool(String) |
Target(SocketAddr) }`. A `Resolver` action with `res.target` set →
`Routed::Target` (wins over `pool`). `proxy.rs` split: `connect_backend` +
`pump` shared; `handle_tcp` (pool, keeps `BackendGuard::observe`) and
`handle_tcp_target(stream, addr, TARGET_CONNECT_TIMEOUT, TARGET_IDLE_TIMEOUT)`
(no guard). `listener_udp::open_session` builds `(backend, Option<BackendGuard>,
idle_ms)` from the `Routed`; `Session._guard` is now `Option<BackendGuard>`;
the sticky table is only written for pool sessions. `sticky_key` from the
response is **not** consumed yet.

### Slice 3 — gRPC transport (done)

`crates/gsp/proto/resolver.proto` (`gsp.resolver.v1.Resolver/Resolve`, messages
mirror the HTTP wire types; absent optionals are `""` / `0`). `crates/gsp/build.rs`
runs `tonic_build` (client + server; server stub is used by the crate test).
`GrpcResolver` in `gsp/src/resolver.rs` holds a `connect_lazy` `Channel` with
`Endpoint::timeout`, builds a `ResolverClient` per call, maps
`Code::DeadlineExceeded → Timeout`. `build_resolvers` now handles
`ResolverKind::Grpc`. New deps: `tonic` + `prost` (in `gsp`), `tonic-build`
(`gsp` build-dep). `#[allow(clippy::result_large_err)]` on the generated `pb`
module (tonic `Status` is large).

### Slice 2 — resolver result cache (done)

`gsp-config`: `resolvers[].cache: { key: [<part>], positive_ttl_sec,
negative_ttl_sec, max_entries }` → `Option<CacheConfig>`; `CacheKeyPart` =
`SrcIp | SrcIpPort | Sni | RoutingKey | FirstBytes(a..=b)` (`first_bytes:a:b`,
`b ≤ PEEK_MAX`).

`gsp-core::resolver::CachedResolver` wraps an `Arc<dyn Resolver>`:
`Mutex<LruCache<String, Entry>>` (`lru` crate), `Entry = Positive { res,
expires } | Negative { expires }`. `key_for(req)` joins the parts with `|`
(`first_bytes` hexed); a `None` from a missing part ⇒ uncacheable ⇒ pass
through. Positive TTL = `res.ttl_sec` or `positive_ttl`. On upstream `Err` with
`on_error == StaleOk`, an expired `Positive` is served. `gsp_resolver_cache_total{resolver,result}`
(`hit | hit_negative | miss | stale | uncacheable`). `gsp/build_resolvers`
wraps `HttpResolver` in `CachedResolver` when `rc.cache.is_some()`. New dep:
`lru` in `gsp-core`.

### Slice 1 — HTTP resolver (done)

`gsp-config`: top-level `resolvers: [{ name, type: http, endpoint, timeout_ms,
on_error }]` → `Config::resolvers: Vec<ResolverConfig>`; `Route.pool: String`
became `Route.action: Action { Pool(String) | Resolver(String) }` (exactly one
of `pool` / `resolver` per action); `OnError { Reject, FallbackRoute, StaleOk }`;
`ListenerConfig::matching_routes(ctx)` (the runtime walks this) and `route_for`
kept as a pool-only convenience; a resolver route forces `peek_len == PEEK_MAX`.

`gsp-core::resolver`: `#[async_trait] trait Resolver { name(); on_error();
resolve(ResolveRequest) -> Result<Resolution, ResolveError> }`,
`ResolveRequest { listener, src, dst, sni, first_bytes, routing_key }`,
`Resolution { pool, target, sticky_key, ttl_sec }`, `type Resolvers =
HashMap<String, Arc<dyn Resolver>>`. `resolve_pool(cfg, resolvers, mctx, first)`
walks `matching_routes`: `Pool` → done; `Resolver` → call, use `pool`, on
error/empty either drop (`reject` / `stale_ok`) or continue (`fallback_route`);
bumps `gsp_resolver_requests_total{resolver,result}`. `Resolvers` is threaded
`Runtime::start(snapshot, resolvers, workers)` → both listeners (like `hints`).

`gsp/src/resolver.rs`: `HttpResolver` (`reqwest`, JSON, `timeout`), a local
base64 encoder, `build_resolvers(&Config)` (called from `main.rs`; `grpc` →
`bail!`). New deps: `async-trait`, `serde_json` in the workspace; `reqwest`
(rustls) + `serde_json` + `async-trait` in `gsp`; `async-trait` in `gsp-core`.
Resolver config is startup-only (not rebuilt on reload).

### Do NOT

- Add QUIC connection-ID awareness (later stage).
- Add cross-instance session handover.
- Add the `regex` crate to `gsp-core` for an inline matcher — regex parsing is a
  Phase 9 plugin concern.

---

## Phase 5 — operations & zero-downtime

Design reference: `docs/06-operations-observability.md` ("Graceful draining &
deployments") and `docs/08` phase 5.

### Slice 1 — backend admin states (`enabled` / `draining` / `disabled`) (done)

`gsp_core::pool::AdminState { Enabled, Draining, Disabled }` — an `AtomicU8` on
`Backend`, orthogonal to the `healthy` flag. `Backend::takes_new_sessions()` =
`is_healthy() && admin_state() == Enabled` is now the selection predicate in
`Pool::acquire_for` (the healthy-set filter) and `Pool::acquire_addr` (UDP
affinity — a drained backend is refused and the caller falls back). Existing
`BackendGuard`s are untouched, so live sessions drain naturally. `Pool::new`
carries the state across a reload by address, next to `carried_healthy`.
`Pool::backend(addr)` is the new lookup helper.

`health.rs` state-gauge loop now emits `gsp_pool_backends{state=draining}` /
`{state=disabled}` (a backend counts as draining/disabled regardless of its
health flag).

`gsp/src/admin.rs`: `PATCH /pools/{pool}/backends/{addr}` with
`{ "state": "enabled" | "draining" | "disabled" }` — resolves the backend in the
live snapshot and calls `set_admin_state`; 404 on unknown pool/backend, 400 on a
bad state string or unparseable `addr`. `AdminState` is imported there as
`BackendState` to dodge the local `struct AdminState` (the axum router state).
`GET /pools` gained a `state=` column. No auth yet (bound to an internal
interface — same as the rest of the admin API).

Not done: `POST` / `DELETE` a backend (needs a snapshot rebuild path outside
`reload.rs`), `GET /config`, `POST /admin/drain`, runtime listener
reconfiguration.

### Slice 2 — graceful connection draining (done)

`gsp_core::drain`: `ConnTracker` (an `Arc` around a `watch::Sender<usize>`) hands
out `ConnGuard`s that `+1` on `track()` and `-1` on `Drop`. `wait_idle()` is a
`watch::Receiver::wait_for(|n| *n == 0)` — immediate when already zero, no
lost-wakeup race.

Wiring: `Runtime` owns one `ConnTracker`, threaded into `run_tcp_listener` /
`run_udp_listener` (like `hints`). The TCP per-conn `tokio::spawn` holds a guard
for the whole connection; each UDP `Session` carries `_conn_guard` (built in
`open_session`).

`Runtime::shutdown()` → `shutdown_with_grace(DEFAULT_SHUTDOWN_GRACE)`;
`shutdown_with_grace(grace)` sends the shutdown watch, then
`timeout(grace, { join all listener/health tasks; conns.wait_idle() })`; on
timeout it logs the leftover count. Finally `t.abort()` on the (already-finished)
listener/health tasks — detached conn tasks are killed by the process exit that
follows. `main.rs` calls `shutdown_with_grace(cfg.shutdown_grace)`.

UDP: the recv loop no longer returns immediately on the signal — it sets a local
`draining` flag, keeps pumping established sessions and running the idle sweep,
refuses new sessions (`gsp_datagrams_dropped_total{reason="draining"}`), and
returns once `sessions.is_empty()`. So `t.await` in `shutdown_with_grace` blocks
on real UDP drain, and the outer `timeout` + `abort()` is the backstop.

Config: `settings.shutdown_grace_sec` (default 30) → `Config::shutdown_grace:
Duration`. `RawSettings` got a hand-written `Default` (the derive gave 0).

Tests use `shutdown_with_grace(100ms)` so they don't wait the 30 s default.

### Slice 3 — instance drain + `GET /config` (done)

`Runtime` / `RuntimeHandle` carry an `Arc<AtomicBool> draining`.
`RuntimeHandle::ready()` = `!is_draining() && !listeners.is_empty()`;
`set_draining(bool)` / `is_draining()`. `shutdown_with_grace` sets the flag
first thing.

`gsp/src/admin.rs`: `POST /admin/drain` (→ `set_draining(true)`, returns
`active_conns`), `POST /admin/undrain` (→ `false`). `readyz` now returns
`draining` (503) vs `not ready` (503) vs `ready` (200). `GET /config` renders a
plaintext dump of the live snapshot — `draining` / `active_conns` header, then
listeners (name, bind, proto, route count, flags) and pools (balancer, backends
with health + admin state + active). No `serde` on the config types — plaintext,
like `/pools`. `/sessions` still not implemented.

### Slice 4 — runtime backend CRUD (done)

`gsp_core::overlay::BackendOverlay` — `Mutex<HashMap<pool, PoolEdits{added,
removed}>>`. `add`/`remove` are idempotent and cancel each other;
`effective_targets(pool, file_targets)` = file order (minus removed) then added
(sorted). `Snapshot::build` now delegates to `build_with_overlay(cfg, prev,
overlay)`, which clones the `PoolConfig` and swaps `targets` only when the
overlay changes them.

`Runtime`/`RuntimeHandle` hold `Arc<BackendOverlay>` + `Arc<Notify>
reload_requested`. `backend_overlay()`, `request_reload()` (`notify_one`),
`reload_requested()` (for `reload.rs` to await). `gsp/src/admin.rs`:
`POST /pools/{p}/backends {addr}` → `overlay.add` + `request_reload`;
`DELETE /pools/{p}/backends/{addr}` → `overlay.remove` + `request_reload`.
`reload::run` selects the Notify alongside SIGHUP / file-watch; `apply` builds
via `build_with_overlay(&cfg, Some(&prev), handle.backend_overlay())`.

**Design note (supersedes the old "only reload.rs builds a Snapshot" caveat):**
there is still exactly one place that builds + stores a snapshot (`reload::apply`);
admin backend edits are just another rebuild trigger, and the overlay is the
persistence layer that keeps them from being undone by a file reload. Backend
*state* (`PATCH`) still mutates atomics on the live `Backend` and needs no
rebuild.

### Slice 5 — runtime listener add / remove / rebind (done, closes phase 5)

`gsp_core::listeners::ListenerManager` — the per-listener accept tasks moved out
of `Runtime::start` into one `Group { cfg, stop: watch<bool>, tasks }` per
listener, keyed by `ListenerConfig::name`. `spawn_group` is the old inner loop
(one task per worker). The groups map is a plain `std::sync::Mutex` that is
never held across `.await`: `reconcile` does phase 1 under the lock (remove
stale groups into a `Vec<Group>`, spawn+insert new ones — all sync), then phase
2 with the lock released (`group.stop().await` = fire the watch, await the
tasks).

`Runtime::start` → `ListenerManager::new(...)` + `start_all(&initial)` (sync,
spawns everything). `reload::apply`: after `handle.store(...)`, when
`cfg.listeners != prev.listeners`, `handle.reconcile_listeners().await` (was a
"needs a restart" warning). `shutdown_with_grace` = `listeners.stop_all().await`
+ await the health task + `wait_idle`, then `abort()` health + `listeners.abort_all()`.

Rebind is stop-old-then-start-new but ordered new-first (phase 1 spawns before
phase 2 awaits the stop), so on a same bind `SO_REUSEPORT` gives a gapless
handover. A listener whose bind is genuinely unavailable spawns a group whose
tasks log an error and exit — same as a startup bind failure; `reconcile` does
not surface it.

### Do NOT (phase 5)

- Build + `store` a `Snapshot` anywhere but `reload::apply`. New runtime edits
  get a persistence layer (like `BackendOverlay`) + a `request_reload()`, they
  do not swap the snapshot themselves.

---

## Phase 6 — client-IP preservation

Design reference: `docs/04-transport-and-client-ip.md` ("Passing the client IP
to the backend") and `docs/08` phase 6.

### Slice 1 — TCP PROXY protocol v1/v2 (done)

`gsp-config`: `ProxyProtocol { None, V1, V2 }` (snake_case), `pools[].proxy_protocol`
(default `none`) → `PoolConfig::proxy_protocol`. `gsp-core::proxy_protocol`:
`header(mode, src, dst, stream) -> Vec<u8>` — v1 is `PROXY TCP4|TCP6 s d sp dp\r\n`
(mixed family ⇒ `PROXY UNKNOWN\r\n`); v2 is the 12-byte sig + `0x21` + family/
transport byte + addr block (mixed family ⇒ `0x20`/`AF_UNSPEC` LOCAL, no addrs).
`Pool::proxy_protocol` carried from cfg. `proxy::handle_tcp(client, peer,
client_local, pool)` writes the header to the backend immediately after
`connect_backend`, before `pump`; a write error is a passive-unhealthy + `Err`.
`listener.rs` passes `local` (already computed for routing).
`gsp_proxy_protocol_headers_total{pool,version}`. Live across reload.
Resolver `target` connections have no pool ⇒ never get a header (open item).

### Slice 3 — TCP transparent mode (done)

`listeners[].transparent: bool`. `bind_reuseport_tcp(.., transparent)` sets
`IP_TRANSPARENT` on the listen socket; `connect_tcp_from(backend, source)` opens
the upstream socket, and with `source` set (same family) sets `IP_TRANSPARENT` +
`bind(source)` before connecting — mismatch / `None` is a plain connect.
`set_ip_transparent<F: AsFd>(&F, v6)` via `socket2::SockRef`,
`#[cfg(target_os = "linux")]` (no `unsafe`). `proxy::connect_backend` /
`handle_tcp` / `handle_tcp_target` carry `transparent_source: Option<SocketAddr>`;
the listener passes `cfg.transparent.then_some(peer)`. Inbound routing is
unchanged — the `IP_TRANSPARENT` listen socket's `getsockname()` already yields
the original destination that `local` / `dst` / `port` use.

### Slice 4 — UDP transparent mode + IPv6 (done)

`UdpMode { Plain, Prefix, Transparent }` selects the recv mechanism. Transparent:
listen socket gets `IP_TRANSPARENT` + `IP_RECVORIGDSTADDR` / `IPV6_RECVORIGDSTADDR`;
`recvmsg_dst` reads `ControlMessageOwned::Ipv4/Ipv6OrigDstAddr` for the full
`ip:port` (prefix mode's `IP_PKTINFO` only carries the dst IP, port = listener).
The session `dst` type widened `Option<IpAddr>` → `Option<SocketAddr>` throughout
(SessionKey, StickyKey, routing `local`). `connect_upstream(backend, source)`
binds the client `ip:port` with `IP_TRANSPARENT` (`bind_transparent_udp`);
`Session._reply_sock` is a per-session `IP_TRANSPARENT` UDP socket bound to the
original destination, and `spawn_reply` sends from it with a plain `send_to`
(prefix mode keeps the `sendmsg` + pktinfo path; plain mode `send_to` on the
shared socket). `bind` failure ⇒ drop reason `reply_bind`. `socket2` 0.5 → 0.6
(`SockRef::set_ip_transparent_v6`, `set_freebind_v4/_v6`; 0.6.5 was already in the
tree via `hyper-util` so `Cargo.lock` moves one line). `transparent` + `prefix`
rejected in `validate()`.

No e2e for either slice — `IP_TRANSPARENT` needs `CAP_NET_ADMIN`, absent in CI
(same as the prefix-mode e2e). Covered: config parse / rejections and
`connect_tcp_from` fallback. Setup recipe (nftables TPROXY tcp+udp + `ip rule`)
in `docs/04`.

### Slice 2 — v2-UDP variant (done)

`ProxyProtocol::V2Udp` (serde `rename = "v2-udp"`). `header()` lost its `stream`
param and switches on the mode (`V2` STREAM, `V2Udp` DGRAM). `validate()`
cross-checks each non-`none` pool against the transport of the listeners that
statically route to it. `listener_udp::open_session` prepends the header to the
first datagram only (`Cow::Owned`); the plain `send(first)` becomes
`send(&first_out)`. `ProxyProtocol::label()` → the metric `version` label.

### Do NOT (phase 6)

- Prepend the header per datagram on UDP — the v2-UDP variant is first-datagram
  only (slice 2).
- Trust an inbound PROXY header from the client — the proxy only ever writes
  headers; accepting them is out of scope.

---

## Codebase map (quick reference)

| File | Responsibility |
|------|----------------|
| `crates/gsp-config/src/lib.rs` | Raw YAML types, `validate()`, resolved `Config`/`PoolConfig`/`ListenerConfig`/`ResolverConfig`/`HealthCheck`; routing (`Matcher`, `Action`, `OnError`, `Cidr`, `HostPattern`, `MatchContext`, `RouteHint`, `extract_sni`). All schema rules here. |
| `crates/gsp-core/src/snapshot.rs` | `Snapshot { listeners, pools }`; `build(cfg, prev)` / `build_with_overlay(cfg, prev, &BackendOverlay)` carry health over and apply admin backend edits. |
| `crates/gsp-core/src/overlay.rs` | `BackendOverlay` — runtime `POST`/`DELETE` backend add/remove edits (`Mutex<HashMap<pool, {added,removed}>>`), layered on the file `targets` at rebuild via `effective_targets`. |
| `crates/gsp-core/src/pool.rs` | `Pool` (balancer + `rr` index + `hash_on`, `acquire` / `acquire_for` / `acquire_addr` / `backend`, `hrw_score`), `Backend` (health/active/streaks/`check_kind` + `AdminState` — `admin_state` / `set_admin_state` / `takes_new_sessions`), `BackendGuard` (RAII slot + passive health), `PickError`. |
| `crates/gsp-core/src/listener.rs` | `run_tcp_listener`: accept loop; per-conn task does first-bytes peek + route match + pool lookup, then metrics + logs. |
| `crates/gsp-core/src/listener_udp.rs` | `run_udp_listener`: per-worker recv loop, `(client, Option<SocketAddr> dst)` session table, sticky affinity, idle sweep, per-session upstream socket + reply pump. `UdpMode` recv: prefix = `recvmsg_dst` + `IP_PKTINFO` + `sendmsg_pktinfo` reply; transparent = `recvmsg_dst` + `IP_ORIGDSTADDR`, client-bound upstream, per-session `IP_TRANSPARENT` reply socket. |
| `crates/gsp-core/src/sniff.rs` | `Sniffer` trait + `sniffer(name)` registry (empty; `#[cfg(test)]` `test-host`) + `warn_if_missing`. The seam for the Phase 9 plugin loader — no built-in sniffers. |
| `crates/gsp-core/src/route_hint.rs` | `RouteHints` — the `ArcSwap<HashMap>` `src_ip → pool` push-resolver table (`POST /route-hint`). Lock-free read. |
| `crates/gsp-core/src/drain.rs` | `ConnTracker` / `ConnGuard` — `watch<usize>` count of live TCP conns + UDP sessions; `wait_idle()` for graceful shutdown. `DEFAULT_SHUTDOWN_GRACE`. |
| `crates/gsp-core/src/resolver.rs` | `trait Resolver`, `ResolveRequest` / `Resolution` / `ResolveError`, `Resolvers` map, `resolve_pool` (the async route walk), `CachedResolver` (TTL LRU). Transports live in `gsp`. |
| `crates/gsp/src/resolver.rs` | `HttpResolver` (`reqwest`), `GrpcResolver` (`tonic`, `mod pb` from `build.rs`), `build_resolvers(&Config)`, a local base64 encoder. |
| `crates/gsp/proto/resolver.proto` + `crates/gsp/build.rs` | The gRPC resolver contract + `tonic_build` codegen. |
| `crates/gsp-core/src/proxy.rs` | `handle_tcp` (pool; writes the pool's PROXY protocol header before the pump) / `handle_tcp_target` (resolver `target`, no guard, no header) → `connect_backend` + `pump` (`copy_with_idle` both ways). |
| `crates/gsp-core/src/proxy_protocol.rs` | `header(mode, src, dst)` — PROXY protocol v1 (text) / v2 (binary, STREAM or DGRAM) encoder. Write-only; parsing is the backend's job. |
| `crates/gsp-core/src/health.rs` | `run`: 500 ms sweep, probes due backends (`tcp_connect` / `udp_probe`), updates health + gauges. |
| `crates/gsp-core/src/runtime.rs` | `Runtime::start(snapshot, resolvers, workers)` builds the `ListenerManager` (`start_all`) + spawns the health task; owns `RouteHints` / `ConnTracker` / `BackendOverlay` / `reload_requested`; `shutdown_with_grace` = `listeners.stop_all` + health await + `wait_idle` + `abort_all`. `RuntimeHandle`: `store` / `ready` / `route_hints` / `backend_overlay` / `request_reload` / `reconcile_listeners` / `active_conns` / `set_draining` / `is_draining`. |
| `crates/gsp-core/src/listeners.rs` | `ListenerManager` — one task `Group` (workers + `watch<bool>` stop) per listener; `start_all` (startup), `reconcile(&Snapshot)` (diff by name → spawn/stop/rebind), `stop_all` / `abort_all`. |
| `crates/gsp-core/src/net.rs` | `bind_reuseport_tcp` (+ `freebind` / `transparent`), `bind_reuseport_udp` (`UdpMode` Plain/Prefix/Transparent), `bind_transparent_udp` (non-local `IP_TRANSPARENT` UDP bind), `connect_tcp_from` (transparent client-bound upstream connect), `set_ip_transparent` (`SockRef`, v4+v6). |
| `crates/gsp-core/src/metrics_defs.rs` | Every metric name. |
| `crates/gsp/src/main.rs` | CLI (`--config`, `--check`), tracing init, runtime bring-up, shutdown. |
| `crates/gsp/src/admin.rs` | axum router: `GET /healthz` `/readyz` `/metrics` `/pools` `/config`, `POST /route-hint`, `PATCH /pools/{pool}/backends/{addr}` (set `AdminState`), `POST /admin/drain` `/admin/undrain`. |
| `crates/gsp/src/reload.rs` | `SIGHUP` + `notify` file watch + `handle.reload_requested()` (admin overlay edits) → debounce → `apply` (validate, `build_with_overlay`, store). |

---

## Open questions carried from `docs/01-requirements.md`

- Does one client ever need **two backends at once** (TCP control + UDP gameplay on
  different instances)? Affects the session model — resolve before phase 3.
- Is **QUIC-aware routing** (connection ID) needed, or is opaque UDP enough? Assume
  opaque for phase 2.
- Cross-instance session failover: assumed **no** for v1.

---

## Infra / environment notes

- Toolchain installed via `rustup` (`stable` 1.98). If `cargo` isn't found:
  `export PATH="$HOME/.cargo/bin:$PATH"`.
- **`protoc` is a build requirement** (gRPC resolver codegen in
  `crates/gsp/build.rs`). Present here (`/usr/bin/protoc`); CI installs
  `protobuf-compiler`; a build box without it fails at `gsp`'s build script.
- CI: `.github/workflows/ci.yml` installs `protoc`, then runs `cargo fmt
  --check`, `clippy --all-targets --all-features`, `cargo test --all`.
- The UDP `prefix:` e2e test (`udp_forward.rs`) needs a Linux host with
  `IP_PKTINFO` and reachable `127.0.0.2` / `127.0.0.3` (both loopback on Linux);
  it is not portable to macOS/Windows CI. In production, prefix mode also needs
  the routed prefix actually routed to the box (and, for a non-local IPv4 base,
  `IP_FREEBIND` / `net.ipv4.ip_nonlocal_bind`).
- `git push` is not possible from this environment. To enable:
  `git remote set-url origin git@github.com:Wueschli/gameserver-proxy.git` (SSH), or
  configure a credential helper / PAT for HTTPS.
- No `LICENSE` decision has been *made* by the user, but `Cargo.toml` declares
  `MIT OR Apache-2.0` and `LICENSE-MIT` / `LICENSE-APACHE` are included to match.
  Confirm this is intended.
