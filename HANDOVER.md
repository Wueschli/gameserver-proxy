# HANDOVER

State of the work, decisions already made, and how to pick it up.
Last updated: 2026-09-04 (**phases 0–9 complete**).
Phase 8 (discovery & scaling): a top-level `backend_sources:` list referenced by
`pools[].source` (exactly one of `targets` / `source`). Kinds: `static` (folded
into the pool's `targets` at load time), `dns_srv`, `consul`, `kubernetes`
(polled Endpoints). `gsp-core::discovery` owns the HTTP-free seam — `trait
BackendSource` (level-triggered `fetch → Vec<SocketAddr>`), a `Discovery`
last-known-good cache, and `refresh_loop` (one control-plane task per source on
`refresh_interval_sec`); concrete adapters (`DnsSrvSource` via
`hickory-resolver`, `ConsulSource` / `KubernetesSource` via `reqwest`) live in
the `gsp` binary, injected into `Runtime::start_with_discovery`. A refresh feeds
`Snapshot::build_with_sources` via the reload task (one snapshot writer);
precedence `discovered set (or file seed) ∪ overlay-added − overlay-removed`,
then health / admin state. An errored / empty refresh keeps the previous set
(never clears the pool) + `gsp_discovery_refresh_total{pool,kind,result}` /
`gsp_discovery_backends{pool}`. HA operations chapter expanded in `docs/06`
(anycast vs. L4 LB, per-instance capacity, dashboards & alerts). Deferred: k8s
watch informer. `backend_sources` live reload landed later (data-plane
completion) — `gsp_core::SourceManager` reconciles the per-pool refresh tasks
on every reload, diffing the new `Snapshot::sources` map.
Filter chain: per-listener radix-trie `allow` / `deny`
CIDR lists + an optional MaxMind GeoIP `geo: { allow, deny }` country filter + a
per-listener `rate_limit` token bucket (per source IP and per /24 / /64) + a
per-listener `per_source` concurrent connection/session cap + process-wide
`settings.limits` caps (`max_connections` / `max_udp_sessions` /
`max_new_sessions_per_sec`) + a UDP `first_packet_gate` (session only on positive
first-datagram recognition); blocked traffic dropped silently +
`gsp_filter_blocked_total{filter=…}` /
`gsp_datagrams_dropped_total{reason="first_packet_gate"}`. Amplifier checklist
covered by `tests/amplification.rs`; `cargo-fuzz` harnesses in
`crates/gsp-config/fuzz/`; `crates/gsp-bench` (`make bench`) measures added
p50/p99 vs. NFR N1/N2.
Phase 5:
`enabled` / `draining` / `disabled` backend states + `PATCH /pools/{p}/backends/{addr}`;
tracked connection draining with `shutdown_grace_sec` on SIGINT/SIGTERM;
`POST /admin/drain` + `GET /config`; runtime backend CRUD; runtime listener
add/remove/rebind. (`GET /sessions` landed later — data-plane completion,
list C.)
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

## Workflow gotcha: always run `cargo fmt --all` as its own step before checking

CI failed a `cargo fmt --all --check` on Phase 9 slice 2
(`crates/gsp-config/src/lib.rs:2142`, a `return Err(Invalid(...))` line rustfmt
wanted wrapped) even though `make check` had reportedly passed locally before
the commit. Root cause: `cargo fmt --all` was run once early in that session,
then more code was added by `Edit` afterward (including the exact line that
broke) without a second unconditional `cargo fmt --all` pass right before
`make check`/commit. `cargo fmt --all --check` (which `make check` /
`fmt-check` run) only *reports* diffs and exits non-zero — it never rewrites
the file — so if the actually-unformatted state is never `cargo fmt --all`ed,
`--check` will (correctly) keep failing, in CI even when a stale local run
looked green. **Rule: immediately before every `make check` / commit, run
`cargo fmt --all` (the writing form, no `--check`) as its own step, not merged
into a pipeline you might skim past** — never assume a fmt pass from earlier
in the session still covers edits made after it. Piping `make check` through
`tail` also hides an early `fmt-check` failure's message even though the
overall command still exits non-zero — don't infer "fmt passed" from tail
output alone; check the exit code or scroll to the top of the log.

---

## TL;DR

- **Planning docs** (`docs/00`–`09`) are complete and in English. They are the design
  source of truth.
- **Code**: Cargo workspace, roadmap **phases 0–7 complete** (this section below
  narrates phases 3–4 in detail; later phases are summarised at the top and in
  `docs/08`). Phase 3 shipped
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
  (resolver) read the form from `resolvers[].proxy_protocol` instead of a pool
  (pre-phase-8 cleanup — see below).
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
  UDP listener, v2-udp rejects a TCP listener, both-transports rejects.
  Resolver-chosen pools are not checked (target pool unknown until runtime —
  runtime falls back to no header: TCP path guards on `V1|V2`, UDP on `V2Udp`).
  (A resolver's own `proxy_protocol` for `target` results *is* transport-checked
  — pre-phase-8 cleanup.)
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
  listener on reload. Matching is a radix trie since slice 6.
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
- **Phase 7 slice 4 done**: UDP first-packet gate. `gsp-config`
  `RawListener::first_packet_gate: bool` → `ListenerConfig::first_packet_gate`
  (UDP-only; `validate()` rejects it on TCP or when the listener has no
  `first_bytes` route and no `sniffer`). `ListenerConfig::first_packet_recognised(&MatchContext)`
  = `sniff` hint present and not `reject`, **or** any `FirstBytes` route matches
  the datagram. `listener_udp::open_session` checks
  `cfg.first_packet_gate && !cfg.first_packet_recognised(&mctx)` right after
  building `mctx` (before the `route_hint` lookup, so a spoofable src_ip hint
  can't bypass it) → `Err("first_packet_gate")` ⇒
  `gsp_datagrams_dropped_total{reason="first_packet_gate"}`. `GET /config` shows
  the `first_packet_gate` flag.
- **Phase 7 slice 5 done**: automated amplifier-checklist tests
  (`crates/gsp-core/tests/amplification.rs`, 4) — no unsolicited / duplicated
  replies; a dropped datagram (routing / ACL / rate / gate) gets no error reply
  and reaches no backend; the client-facing reply is byte-for-byte the backend
  payload (proxy prepends nothing toward the client); the rate limit is enforced
  before any session / forward. The `docs/07` checklist is now ticked and
  points at the test names. Test-only slice, no production change.
- **Phase 7 slice 6 done**: ACL longest-prefix-match trie. `gsp_config::CidrSet`
  — a binary radix trie over address bits, v4 / v6 kept in separate roots, with
  `build(&[Cidr])` / `contains(ip)` / `len()` / `is_empty()`. `TrieNode {
  terminal, children: [Option<Box>; 2] }`; `insert` stops at a shorter covering
  prefix and clears children a longer one would need (subsumption); `walk`
  early-exits on the first `terminal` on the path (membership, not full LPM).
  `Acl` now holds `allow_set` / `deny_set: CidrSet` alongside the `Cidr` vecs
  (kept for `PartialEq` reload-diffing — hand-written `impl` over the vecs — and
  for `GET /config`'s `+N/-M`); `Acl::new(allow, deny)` compiles them;
  `Acl::permits` walks the tries. No config / semantics / metric change. Built
  in `validate()` (so it rides the existing `ListenerConfig` clone into
  `gsp-core`; no new gsp-core plumbing).
- **Phase 7 slice 7 done**: optional GeoIP country filter. `gsp-config`:
  `settings.geo_db: Option<String>` → `Config::geo_db` / `Snapshot::geo_db`;
  per-listener `geo: { allow, deny }` (ISO 3166-1 alpha-2, upper-cased at parse)
  → `ListenerConfig::geo: Option<GeoAcl>`. `GeoAcl::permits(Option<[u8;2]>)`
  mirrors `Acl`: `deny` wins, non-empty `allow` is default-deny, `None` country
  admitted only when `allow` is empty. `validate()` requires `settings.geo_db`
  when any listener has `geo`, a non-empty allow/deny, and 2-letter codes.
  `gsp-core::geo::GeoDb` wraps `maxminddb::Reader<Vec<u8>>`
  (`open` + `country_code(ip) -> Option<[u8;2]>` via `decode_path(&path!["country",
  "iso_code"])`). `Runtime::start_with_geo(snapshot, resolvers, Option<Arc<GeoDb>>,
  workers)` (plain `start` delegates with `None`, keeping ~29 call sites intact);
  the `gsp` binary opens the DB in `main.rs` and fails `--check` / startup if the
  path is bad, threading `Option<Arc<GeoDb>>` through `ListenerManager` → workers.
  Checked in `listener.rs` / `listener_udp.rs` right after the CIDR ACL:
  `geo.as_deref().map(|db| geo_acl.permits(db.country_code(ip))).unwrap_or(false)`
  — **fail-closed** if the DB isn't loaded. `m::FILTER_BLOCKED` gains
  `filter="geo"`. `GET /config` shows `geo_db=<path>` and per-listener
  `geo=+N/-M`. `geo:` codes reload (respawn); `geo_db` path is startup-only.
  Test fixture: `crates/gsp-core/tests/data/GeoIP2-Country-Test.mmdb` (MaxMind's
  Apache-2.0 synthetic test DB, see the data `README.md`).
- **Phase 7 slice 8 done**: `cargo-fuzz` harnesses in `crates/gsp-config/fuzz/`
  (its own standalone workspace — `[workspace]` in the fuzz `Cargo.toml`, so the
  sanitizer build never touches the main one). Targets: `extract_sni`
  (`gsp_config::extract_sni` on the peek buffer), `route_match`
  (`route_for` / `first_packet_recognised` with a fuzzed first-bytes buffer over
  a fixed matcher set — the config is built once via `OnceLock`), `parse_config`
  (`parse_str` on arbitrary UTF-8). Seeds in `fuzz/seeds/<target>/`
  (a real ClientHello, `config.example.yaml`, an A2S query). `make fuzz`
  (`FUZZ_TIME=<sec>`, needs nightly + `cargo install cargo-fuzz`; run as
  `cargo +nightly fuzz …` — `+nightly` overrides the repo's stable
  `rust-toolchain.toml`). New CI job `fuzz` (nightly, builds + 45 s smoke-run
  per target). Initial runs: no crashes (10.9M execs on `extract_sni`, 2.6M on
  `route_match`, ~35k on the heavier `parse_config`).
- **Phase 7 slice 9 done**: per-source concurrent connection / session cap.
  `gsp-config` `RawListener::per_source` → `Option<PerSourceLimit { max_per_ip,
  max_per_net: Option<usize> }>` (`ListenerConfig::per_source`); `validate()`
  rejects a `0` and an empty `per_source`. `gsp-core::src_conns::SourceLimiter`
  — `Mutex<{ ips: HashMap<IpAddr,usize>, nets: HashMap<NetKey,usize> }>` (`NetKey`
  reused from `ratelimit`, now `pub(crate)`); `acquire(ip) -> Result<SourceGuard,
  &'static str>` checks `ip < max_ip && net < max_net`, increments both on
  success, consumes nothing on refusal; `SourceGuard::drop` decrements (removing
  a zeroed entry). One `Arc<SourceLimiter>` per listener built in
  `spawn_group`, cloned to workers, rebuilt on respawn. `run_tcp_listener` /
  `run_udp_listener` gained `src_limiter`; checked right after the `rate_limit`
  bucket — TCP holds the guard in the per-conn task, UDP stores it on `Session`
  (drops on eviction). `m::FILTER_BLOCKED` gains `src_conn_ip` / `src_conn_net`.
  `GET /config` shows `per_source=ip:N,net:N`. **No LRU eviction under pressure**
  — a full source is simply refused until the idle sweep / connection close
  frees a slot (noted in `docs/07`).
- **Phase 7 slice 10 done — phase 7 complete**: `crates/gsp-bench` (new
  workspace member, `gsp-bench` → `gsp-core`, tool only). `make bench`
  (`BENCH_ARGS=...`) spins up an in-process echo backend + `Runtime`, times many
  sequential request→response round-trips direct vs. through the proxy, and
  reports `added p50/p99 = proxy − direct` with `PASS`/`MISS` vs. NFR N1
  (`< 0.5 ms`) / N2 (`< 2 ms`). Flags: `--protocol tcp|udp|both`,
  `--iterations`, `--payload`, `--connections N` (N extra busy conns for
  contention — the timed conn is separate; only meaningful at low background
  load), `--workers`, `--strict` (exit 1 on MISS). Also prints informational
  single-stream throughput and (with `--connections ≥ 1000`) idle RSS per
  connection. Loopback numbers on this box: added p50 ≈ 10 µs, p99 ≈ 30 µs.
  N3/N4/N5/N9 need real hardware + a load generator (noted in `docs/06` and the
  crate README).
- **Pre-phase-8 cleanup (done)**: UDP passive health on ICMP port-unreachable.
  A connected upstream UDP socket that draws an ICMP port-unreachable reports
  `ConnectionRefused` on `send` (steady-state forward path) or `recv` (reply
  pump); both now call `Backend::observe(false)` so a dead UDP backend is marked
  unhealthy from the data path instead of waiting for the active `udp_probe`
  sweep. `BackendGuard::backend()` hands the reply task an `Arc<Backend>` (the
  guard itself stays on the `Session`); `Session.health` / `spawn_reply`'s
  `health` param carry it. Resolver `target` sessions have no backend ⇒ no-op.
  Test: `udp_forward::icmp_port_unreachable_marks_the_backend_unhealthy`.
- **Pre-phase-8 cleanup (done)**: client-IP preservation on resolver `target`
  connections. New `resolvers[].proxy_protocol: none | v1 | v2 | v2-udp`
  (`RawResolver` → `ResolverConfig::proxy_protocol`); `validate()` cross-checks
  it against the transport of the listeners with a `resolver` route to it, same
  rule as pools (v1/v2 ⇒ TCP-only, v2-udp ⇒ UDP-only). `Routed::Target` went
  `Target(SocketAddr)` → `Target { addr, proxy_protocol }`; `Resolver` trait
  gained `fn proxy_protocol(&self) -> ProxyProtocol` (default `None`, forwarded
  by `CachedResolver`, set from config by `HttpResolver` / `GrpcResolver`).
  `resolve_route` stamps `resolver.proxy_protocol()` onto the `Target`.
  `proxy::handle_tcp_target` now takes `client_addr` / `client_local` /
  `proxy_protocol` and writes a v1/v2 header before the pump, exactly like
  `handle_tcp`; `listener_udp::open_session` uses the carried form for its
  existing v2-udp first-datagram path. Metric label
  `gsp_proxy_protocol_headers_total{pool="(resolver target)",version}`. Push-hint
  targets and direct-config targets carry `ProxyProtocol::None` ⇒ no header (as
  before). Tests: `resolver::resolver_target_gets_a_proxy_protocol_header`
  (e2e), gsp-config parse + two transport-mismatch rejections.
- **Phase 9 slice 1 done**: sniffer registry threading (pure refactor, no
  behaviour change). `gsp_core::sniff::Sniffers` — a
  `HashMap<String, Arc<dyn Sniffer>>` registry (`register` / `get`) replacing
  the old global `sniff::sniffer(name)` fn. Held as `Arc<Sniffers>` on
  `Runtime` and threaded `ListenerManager::new` → `spawn_group` →
  `run_tcp_listener` / `run_udp_listener` → (UDP) `open_session`, exactly like
  `Arc<Resolvers>` / `Option<Arc<GeoDb>>`. `warn_if_missing` now takes the
  registry. New `Runtime::start_with_sniffers(initial, resolvers, geo,
  sniffers, workers)` (between `start_with_geo` and `start_with_discovery`,
  which grew a `sniffers` param — `gsp/src/main.rs` passes
  `Sniffers::default()`, i.e. still no built-ins). Tests that need the seam
  build a small registry (`sniff::tests::test_registry()`, holds `test-host`)
  instead of relying on a global match arm.
- **Phase 9 slice 2 done**: `settings.sniffers` config schema (loader itself is
  still later slices — this is config-only, nothing reads it yet).
  `gsp_config::SniffersConfig { dir, call_timeout, max_memory_bytes, modules:
  Vec<SnifferModulePin { name, sha256 }> }` on `Config::sniffers: Option<...>`
  (`None` = today's behaviour — no plugins, a `sniffer:` route never matches).
  Raw `RawSniffers { dir, call_timeout_ms (default 20), max_memory_bytes
  (default 16 MiB), modules }`; `validate_sniffers` rejects an empty `dir`, a
  zero timeout / memory cap, an empty module `name`, and a `sha256` that isn't
  64 hex chars (normalised to lowercase on success). `config.example.yaml` has
  a commented example block; `docs/05` documents the schema, a validation
  bullet, and a reload-semantics row (restart-only until slice 4's `dir`
  rescan). 6 new `gsp-config` tests.
- **Phase 9 slice 3 done**: `WasmSniffer`, the real plugin loader
  (`crates/gsp/src/sniffer_loader.rs`). Shared `wasmtime::Engine` (epoch
  interruption on) + one epoch-ticker thread; a fresh `Store` + `Instance` per
  call with a `StoreLimits` memory cap and a one-tick epoch deadline
  (`settings.sniffers.call_timeout_ms`). ABI: guest exports `memory`,
  `alloc(len) -> ptr`, `sniff(ptr,len) -> packed|0` (widened later to
  `sniff(in_ptr, in_len, cfg_ptr, cfg_len)` by ADR 16a — per-plugin config);
  host marshals the peeked bytes in, decodes a compact `RouteHint` (flags byte +
  length-prefixed UTF-8 strings) out — any bad pointer/length/UTF-8 is
  `bad_output`, never a panic.
  `Sniffer::name` is now `&str` (was `&'static str` — a plugin's name is its
  file stem). `build_sniffers(&SniffersConfig)` scans `dir`, verifies
  `sha256` pins (`sha2` dep) when configured, returns a `Sniffers` registry;
  `main.rs` calls it for both `--check` and startup and fails closed on a bad
  plugin (same pattern as `geo_db`). New metrics
  `gsp_sniffer_calls_total{name,result}` / `gsp_sniffer_call_seconds{name}`.
  6 new tests in `sniffer_loader.rs`, including a hand-written WAT fixture
  (via the `wat` dev-dep — no `wasm32-unknown-unknown` toolchain needed) that
  proves the epoch deadline actually traps an infinite-loop plugin instead of
  hanging. `make check` green (gsp: 11 tests now).
- **Phase 9 slice 4 done**: live `dir` rescanning on reload.
  `gsp_core::sniff::Sniffers` now holds its name→plugin map behind an
  `ArcSwap` (mirroring `route_hint::RouteHints`) instead of a plain
  `HashMap`, giving it interior mutability: every `Arc<Sniffers>` clone handed
  to a listener worker at spawn time is the *same* instance, so
  `Sniffers::replace(map)` is visible to every worker immediately with no
  replumbing through `Runtime`/`ListenerManager` (unlike the snapshot swap,
  which every worker re-reads via its own `ArcSwap<Snapshot>::load`).
  `Sniffers::get` now returns an owned `Arc<dyn Sniffer>` (was
  `&Arc<dyn Sniffer>`) so a caller never holds a borrow into a table that can
  be swapped from under it — every existing call site kept working unchanged
  since the `.and_then(|s| s.sniff(...))` chain derefs either way.
  `crates/gsp/src/sniffer_loader.rs` split: `SnifferLoader` now holds only the
  shared `wasmtime::Engine` + its epoch-ticker thread (built once, in
  `SnifferLoader::new`) and exposes `scan(&SniffersConfig) ->
  HashMap<String, Arc<dyn Sniffer>>`; `build_sniffers` (the startup path,
  still used by `--check` and process start) is now `SnifferLoader::new` +
  one `scan`. `main.rs` keeps the `Arc<SnifferLoader>` alive and passes it
  (plus the live `Arc<Sniffers>`) into `reload::run`, which now takes both and
  calls a new `rescan_sniffers` after every applied config load: a scan error
  keeps the previous plugin set (never half-applies, same spirit as
  discovery's last-known-good); `settings.sniffers` appearing or disappearing
  between reloads (no loader built at startup, or vice versa) is logged as
  needing a restart, not attempted live. `call_timeout_ms` / `max_memory_bytes`
  stay startup-only — only the *contents* of `dir` are live, matching the
  locked `docs/08` plan. 1 new test
  (`sniffer_loader::tests::scan_reflects_added_and_removed_modules`, drives
  `SnifferLoader::scan` + `Sniffers::replace` directly the way the reload task
  does — add a module, rescan, see it; remove it, rescan, see it gone).
  `make check` green (gsp: 12 tests now).
- **Phase 9 slice 5 done**: first-party plugin crates. New standalone
  workspace `crates/plugins/` (own `[workspace]` in its `Cargo.toml`, exactly
  like `crates/gsp-config/fuzz` — never a member of the root workspace, never
  a dependency of `gsp`/`gsp-core`, so the main `make check` needs no wasm
  toolchain). `gsp-sniffer-abi`: the guest-side write half of the ABI
  `sniffer_loader.rs` documented in slice 3 — `alloc(len) -> ptr` delegates to
  the module's own global allocator (`std::alloc::alloc`, not a hand-rolled
  bump pointer — matters, see the "gotcha" below), `encode(&Hint) -> Vec<u8>`
  is pure and unit-tested on any host target, `emit_hint` places `encode`'s
  bytes via `alloc` and packs the result. Three plugins, each `crate-type =
  ["cdylib", "lib"]` so `recognise(&[u8]) -> Option<Hint>` is an ordinary
  native-testable fn and `sniff` a thin `#[no_mangle] extern "C"` wrapper:
  - `a2s`: Source-engine (CS:GO/TF2/GMod/Rust/…) A2S query packets —
    `0xFFFFFFFF` + a query-type byte (`T`/`U`/`V`/`W`); no hostname, tags
    `key: "a2s"`.
  - `minecraft`: parses a protocol ≥ 1.7 Handshake packet's VarInt-prefixed
    fields and extracts the `server address` string as `host` — the same
    virtual-host trick BungeeCord/Velocity use; strips a Forge `\0FML\0…`
    suffix, lower-cases to match the proxy's `sni`-style `host:` patterns.
  - `regex-firstbytes`: **a template, not a generic engine** — see the
    known-limitation note below. Hard-codes an HTTP/1.x request-line matcher
    (`GET /x HTTP/1.1\r\n`-shaped) to demonstrate the bounded, allocation-light
    matcher shape the roadmap describes, tags `key: "http"`.

  `make plugins` = native `cargo test --workspace` (13 tests: 4 in
  `gsp-sniffer-abi`, 2 in `a2s`, 5 in `minecraft`, 2 in `regex-firstbytes`)
  then `cargo build --release --target wasm32-unknown-unknown -p a2s -p
  minecraft -p regex-firstbytes`. Release profile (`opt-level = "z"`, `lto =
  true`, `panic = "abort"`, `strip = true`) → ~17–21 KiB per module. New CI
  job `plugins` (installs `wasm32-unknown-unknown`, runs `make plugins`'s two
  steps, prints sizes). `crates/plugins/README.md` has the full
  build/install walkthrough (copy the `.wasm` into `settings.sniffers.dir`,
  named `<sniffer-name>.wasm` — note `regex-firstbytes` → file
  `regex_firstbytes.wasm`, Cargo's usual `-`→`_` crate-name-to-filename
  rule). One new `#[ignore]`d integration test,
  `gsp::sniffer_loader::tests::first_party_plugin_artifacts_recognise_their_protocols`
  (run with `cargo test -p gsp plugin_artifacts -- --ignored` *after*
  `make plugins`) — loads the actual built `.wasm` files (not a hand-WAT
  fixture) through the real `WasmSniffer`/`wasmtime` path for all three
  plugins, proving the guest ABI encoder and the host decoder from slice 3
  actually agree, not just that each side's own unit tests pass in isolation.
  **Gotcha this test caught**: a real `std`-linked wasm module's default
  allocator wants ~1.1 MiB of linear memory just to instantiate (17 pages);
  the small hand-WAT slice-3 test fixtures never needed more than one or two
  pages, so a `max_memory_bytes` picked to fit *those* (1 MiB) silently fails
  every real plugin at `Instance::new` — the shipped `config.example.yaml`
  default (16 MiB) is comfortably above this, but it's worth remembering when
  hand-picking a tighter cap.
  **Known limitation at the time** — since resolved by the data-plane-completion
  "per-plugin config" slice (ADR 16a): there was no per-plugin configuration
  path, so `regex-firstbytes` could not be a genuinely runtime-configured
  matcher. `settings.sniffers.modules[]` now carries an optional `config` string
  handed to the plugin via the widened `sniff(in_ptr, in_len, cfg_ptr, cfg_len)`
  ABI.
- **Phase 9 slice 6 done**: WASM-boundary latency bench vs. NFR N1.
  `sniffer_loader::tests::wasm_boundary_latency_vs_nfr_n1` in
  `crates/gsp/src/sniffer_loader.rs` — an `#[ignore]`d test (same convention
  as slice 5's artifact test) rather than a `gsp-bench` extension (that crate
  only depends on `gsp-core`; the loader is `gsp`-binary-only, so reusing the
  loader's own test module avoided a new cross-crate seam) or `criterion`
  (kept the project's existing custom-harness style). Times the real,
  compiled first-party plugins — not synthetic WAT fixtures — through the
  actual `alloc`/`memory.write`/`sniff`/decode round trip, 2000 calls each
  after warmup, sorted for p50/p90/p99/max, PASS/MISS vs. N1 (< 0.5 ms), same
  reporting shape as `gsp-bench`. Run with `cargo test -p gsp --release
  wasm_boundary -- --ignored --nocapture` after `make plugins` (release
  matters — Cranelift codegen and the sandboxed call are both much slower
  unoptimised).
  **Result: p50 8–10 µs, p99 12–26 µs across all three plugins — comfortably
  under N1**, so the fresh-`Store`-per-call design from slice 3 needs none of
  the `InstancePre` / pooling-allocator / warm-instance fallbacks the locked
  decision held in reserve.
  **A real finding along the way**: the shared epoch-ticker thread (slice 3)
  runs on wall-clock time for the whole loader's lifetime, so a call that
  happens to straddle a tick boundary legitimately traps even though it only
  took ~10 µs of actual work (confirmed by instrumenting — a plugin looped in
  isolation failed deterministically at the same point every run, right
  around one `call_timeout_ms` period of elapsed wall time). This is the
  epoch-interruption mechanism working exactly as designed, not a bug — but
  it means `call_timeout_ms` is a *ceiling*, not a guarantee every call gets
  the full budget; a call starting just before a tick gets whatever's left.
  The bench uses a long `call_timeout` to get a clean latency measurement
  isolated from this effect (it's already covered separately by
  `wasm_plugin_call_times_out_under_the_epoch_deadline` from slice 3).
  New `docs/07` "Sniffer plugin sandbox guarantees" section: the full
  no-WASI/no-host-imports contract (what a plugin provably cannot do and
  why), the two runtime bounds (epoch timeout + `StoreLimits` memory cap)
  with the ceiling caveat above, the `sha256` pin supply-chain check, the
  latency table, and a note that a sniffer's lack of a reply path leaves the
  amplifier checklist untouched.
- **Phase 9 slice 7 done — phase 9 complete**: end-to-end test through the
  real loader.
  `sniffer_loader::tests::end_to_end_connection_routes_by_a_real_wasm_plugins_hint`
  compiles the same `HOST_SNIFFER_WAT` fixture slice 3's own tests use (inline
  via the `wat` crate — no `wasm32-unknown-unknown` toolchain needed), writes
  it to a scratch dir as `host-echo.wasm`, and has `build_sniffers` load it
  the *real* way (a directory scan, not a hand-built `WasmSniffer`), then
  drives a live `Runtime` + a real TCP connection routed by the plugin's
  returned hint host — mirrors
  `sniff::tests::sniffer_matcher_routes_a_connection_by_hint_host` (the
  native-sniffer version from slice 1) but through the whole WASM path. Not
  `#[ignore]`d — runs in every normal `cargo test -p gsp` / `make check`.
  **A small gotcha it caught while writing it**: `HOST_SNIFFER_WAT` takes
  *everything after* `HOST:` as the hostname (documented in its own comment)
  rather than stopping at a `\n` like the native `TestHost` sniffer does — an
  initial version of this test sent `HOST:<host>\nrest` (matching the native
  sniffer's test payload shape) and got a host of `"<host>\nrest"` back,
  which then failed to match the route's `host:` pattern and silently fell
  through to the `always` route. Fixed by sending a bare `HOST:<host>` with
  no trailing bytes, with a comment on why — the fixture's behaviour is by
  design, not a bug, but it's easy to trip over by analogy with the native
  fixture.
  **CI wiring**: the `plugins` job now also installs `protoc` and, after
  building the wasm modules, runs `cargo test -p gsp --release -- --ignored
  --nocapture` — so slice 5's artifact round-trip test and slice 6's N1
  latency bench are both enforced on every push/PR, not just documented as
  locally reproducible.
  `docs/08` Phase 9 marked **complete**: 3 risks resolved (CI wasm-target
  install — done; epoch-ticker thread — documented in `docs/07`; warm-instance
  reuse — turned out unnecessary, slice 6 measured comfortably inside N1).
  Known follow-ups, not blocking: per-source cap LRU eviction, `GET
  /sessions`, a k8s discovery watch informer, resolver `sticky_key`.
  (Per-plugin configuration — `settings.sniffers.modules[].config` + the
  widened `sniff` ABI — shipped later, in the data-plane-completion pass; see
  ADR 16a.)
- **Next**: phase 9 is done. Pick up from `docs/08`'s remaining phases
  (10–12, the distributed control plane — design only, nothing built yet;
  see `docs/10-distributed-control-plane.md`) or one of the polish items
  above, per what the user wants next.
- **Post-phase-9 tooling: `gsp-bench` concurrency ramp (not tied to a roadmap
  phase)**. Prompted by a direct question about whether the proxy had ever
  been load-tested under real concurrency — it hadn't; `latency` mode runs
  client+proxy+backend in one process/runtime, which structurally can't
  exercise accept-queue backpressure, fd/allocator behaviour, or give an
  honest RSS/fd reading for the proxy alone. Added a second `gsp-bench` mode,
  `concurrency` (`crates/gsp-bench/src/concurrency.rs`, shared helpers moved
  to a new `common.rs`): builds the real `gsp` binary (`cargo build --release
  -p gsp`), spawns it as a genuine separate OS process against a generated
  config, and ramps a real client-held connection count through `--steps`
  (TCP and/or UDP), sampling the proxy child's own RSS/open-fd count
  (`/proc/<pid>/...`, meaningful now that it's not shared with the harness's
  own sockets) and added-latency percentiles at each step.
  **Deliberately scoped down from the full N4/N5 targets** (see the earlier
  conversation): reaching 500k connections needs multiple client source
  addresses (one client process is capped by its ephemeral-port range —
  `ip_local_port_range`, ~28k on this box) and would produce a number that
  *looks* like NFR validation without being one — still loopback, still one
  container. Verified working at **20,000 concurrent real TCP connections**
  and **5,000 concurrent UDP sessions** through the real proxy process on
  this box (RSS ~426 MiB / 40k fds at 20k TCP conns, ~21 KiB/conn; added p50
  stayed 5–15 µs throughout the ramp, no NFR N1/N2 degradation under load).
  **A real bug this surfaced and fixed**: the in-process echo backend used a
  plain `TcpListener::bind`, which gets the OS default `listen()` backlog
  (128) — once the ramp burst hundreds of new proxy→backend connects at once,
  the *backend's* accept queue (not the proxy) became the bottleneck, timing
  out the proxy's 300 ms `connect_timeout_ms` and flapping the backend
  passively unhealthy (visible as `backend health changed (passive)
  healthy=false` / `no healthy backend in pool p` in the proxy's own logs,
  found by temporarily un-suppressing the child's stdio). Fixed by binding
  the backend echo listener via `socket2` with an explicit backlog of 4096,
  matching what `gsp-core`'s own `bind_reuseport_tcp` already does for real
  listeners — new `socket2` dep for `gsp-bench` (already a workspace dep via
  `gsp-core`). Also fixed a client-side accounting gap: the original
  `established` counter only recorded "connect to the proxy succeeded",
  not "still alive" — a held TCP connection now `select!`s on the stop
  signal vs. a read (which can only fire on peer-close for a connection we
  never write to), decrementing `established` / incrementing `failed` on an
  unexpected death, so the reported count is a live count, not a
  once-ever-connected count. `crates/gsp-bench/README.md` documents both
  fixes and the scope caveats. `docs/06` capacity-planning section and this
  entry are the only doc touches — this isn't a roadmap phase, just a direct
  answer to "can we simulate load without dedicated hardware".
- **Roadmap extended**: `docs/10-distributed-control-plane.md` (new) designs the
  v2 distributed control plane — Tier 1 global config/intent store + a
  `gsp-controller` + web UI (phases 10–11), Tier 2 regional health gossip
  (phase 12). Design only, nothing built; the data plane and the
  immutable-`Snapshot` invariant are explicitly untouched (every mechanism is
  just another writer into the existing `validate → build → ArcSwap::store`
  path, or another input to the health flag). New ADRs 4a / 13 / 14 / 15 in
  `docs/09`; milestone cuts v1.3 (phase 9–10) and v2.0 (phase 11–12) added to
  `docs/08`.
- **Build/verify**: `make check` (fmt + clippy `-D warnings` + ~150 tests). Needs
  `protoc` on `PATH` (gRPC codegen in `crates/gsp/build.rs`). `wasmtime`
  (phase 9 slice 3) is a normal `cargo` dependency — it does not need the
  `wasm32-unknown-unknown` rustc target; that target is only needed later
  (phase 9 slice 5) to *build* first-party plugins, not to run the loader or
  its tests (which use the `wat` crate to assemble WASM bytes from inline text
  at test time).
- **Infra**: git repo, remote `github.com/Wueschli/gameserver-proxy`, branch `main`.
  `git push` works again (through slice 7); `origin/main` is current. The HTTPS
  credential helper still logs a harmless "nonexistent Windows path" warning
  before falling back to a working credential.

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
  it already holds. A ClientHello split across TCP segments is reassembled (the
  path re-peeks every 5 ms until the whole first TLS record is buffered or the
  250 ms budget runs out), so `sni` still matches a multi-segment ClientHello. A
  silent TCP client (or one that stalls mid-handshake past the budget) routes as
  if it sent nothing.
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
  `weighted` (weighted round-robin — pool `weights: { "ip:port": N }`, default 1,
  works with a discovered `source:` too),
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
  Resolver `target` connections take the form from `resolvers[].proxy_protocol`.
  Alternatively, a TCP or
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
- `gsp-core/tests/amplification.rs` (4): the `docs/07` amplifier checklist — no
  unsolicited / duplicated replies (silent client + bystander hear nothing, one
  request → one reply); a `no_route` datagram gets no error reply and reaches no
  backend; the client-facing reply is exactly the backend payload (proxy adds
  nothing); a low `rate_limit` rejects datagrams before any session / forward.

- `gsp-core/tests/data/GeoIP2-Country-Test.mmdb` — MaxMind's Apache-2.0
  synthetic test DB (see the sibling `README.md`); used by `geo::tests` and by
  `tcp_forward::geo_filter_denies_unlisted_country` (a listener with
  `allow: [SE]` fails a loopback client closed, no geo admits it).

(The per-file counts above predate phases 3–7; `make check` runs ~142.)

**Fuzzing** (`crates/gsp-config/fuzz/`, not part of `make check`): `make fuzz`
runs `extract_sni` / `route_match` / `parse_config` for `FUZZ_TIME` seconds each.
Needs nightly + `cargo-fuzz`. CI runs a 45 s smoke pass per target.

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
| Balancers | `round_robin` (atomic index + `rotate_left`), `least_conn` (sort healthy by active), `weighted` (weighted round-robin: one atomic tick indexes the cumulative-weight line; pool `weights` map, default 1), `consistent_hash` (rendezvous/HRW hash via `std` `DefaultHasher`; no `hashring` dep — backend set is tiny). |
| UDP | Worker-local session table (no global lock), `connect(2)` socket + reply task per session, per-worker sticky affinity table (hard cap, wholesale clear), 1 s idle sweep. `recvmmsg`/`sendmmsg`, timing wheel deferred. See ADR 9. `consistent_hash` now gives table-free affinity as an alternative to the sticky table. |
| UDP prefix routing | One wildcard `IP_PKTINFO` socket per prefix (`recvmsg` for the real dest, `sendmsg` cmsg for the reply source), via `nix` — zero `unsafe`. See ADR 10. |
| Discovery adapters | **done** (phase 8): `BackendSource` seam + `Discovery` + `refresh_loop` in `gsp-core`; `DnsSrvSource` (`hickory-resolver`) / `ConsulSource` / `KubernetesSource` (`reqwest`) in `gsp`. Level-triggered, last-known-good on failure, fed through `Snapshot::build_with_sources`. |
| Sniffers | Loader **done** (phase 9 slices 3–5): `wasmtime`, core WASM module (no WASI), epoch interruption + `StoreLimits` for the two bounds; `settings.sniffers.dir` is rescanned live on reload (an `ArcSwap`-backed `Sniffers` registry, swapped like the snapshot — engine params are startup-only). `wasmtime`/`sha2` are binary-only deps (`gsp` only) — `gsp-core` still only has the `Sniffer` trait / `Sniffers` registry. First-party plugins (`a2s`, `minecraft`, `regex-firstbytes`) + the `gsp-sniffer-abi` guest helper live in the standalone `crates/plugins/` workspace, `make plugins`. Per-plugin config (ADR 16a): the guest `sniff` export takes `(in_ptr, in_len, cfg_ptr, cfg_len)`; `settings.sniffers.modules[].config` is a string marshalled into a second linear-memory region on every call (`WasmSniffer` holds it; the `gsp-core` trait is unchanged). |
| PROXY protocol (`proxy_protocol: v1 / v2 / v2-udp`) + TPROXY transparent mode (`transparent: true`, TCP + UDP) | **done** (phase 6). `set_ip_transparent` via `socket2` 0.6 `SockRef`; origdst via `nix` — still zero `unsafe`. |
| External resolver | `trait Resolver` + cache + `on_error` + routing loop in `gsp-core`; HTTP/gRPC clients in the `gsp` binary, injected as `Arc<dyn Resolver>` (same pattern as the sniffer seam). Keeps HTTP out of `gsp-core`. The `Resolvers` registry is `ArcSwap`-backed (like `Sniffers`) so a `resolvers:` change reloads live — rebuilt + swapped by the reload task when `ResolverConfig` differs (resets the LRU caches). |
| Deps kept out of `gsp-core` | `axum`, `clap`, `notify`, `reqwest`, `hickory-resolver`, `wasmtime`, `sha2` live in the `gsp` binary only. (`gsp-core` uses `nix` for `IP_PKTINFO` / `IP_ORIGDSTADDR` cmsgs, `socket2` 0.6 for `IP_TRANSPARENT` / `IP_FREEBIND`, `async-trait` for `Resolver`, `lru` for the resolver cache, and `maxminddb` — a pure-Rust `.mmdb` reader, no network — for the geo filter.) |

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
| `weighted` balancer | **done** (data-plane completion, item 5 — `balancer: weighted` + pool `weights: { "ip:port": N }`, weighted round-robin in `Pool::acquire_for`) |
| `first_available` balancer | later |
| UDP ICMP port-unreachable as an explicit passive health signal | **done** (pre-phase-8 cleanup) |
| Listener add / remove / rebind at runtime | **done** (phase 5 slice 5) |
| Tracked connection drain with a grace period on shutdown | **done** (phase 5 slice 2) |
| CRUD admin API: `POST` / `DELETE` a backend, `GET /config`, `POST /admin/drain`, `PATCH` backend state | **done** (phase 5 slices 1, 3, 4) |
| `GET /sessions` introspection (needs a per-session registry) | phase 5 / later |
| `draining` / `disabled` backend states | **done** (phase 5 slice 1) |
| Reload debounce only coalesces within one 200 ms window; wider-spaced events cause separate (idempotent) reloads | polish, low priority |
| **Phase 3 — done** (slices 1–9): route rule list; matchers `always` / `client_cidr` / `dst` / `port` / `first_bytes` (`prefix`+`length`) / `sni`; `consistent_hash` balancer; UDP `prefix:` listener + TCP `freebind:`; sniffer API seam + `sniffer` matcher (no built-ins); `POST /route-hint` push resolver | **done** |
| Sniffer plugin **loader** + a generic `first_bytes` `regex` matcher (as a plugin) — separate community repo, sandboxed/WASM, runtime-loaded | **Phase 9** |
| `first_bytes` `regex` variant (needs `regex` dep; goes in the `gsp_core::sniff` layer, not `gsp-config`) | phase 3 |
| More sniffers (`quic`, `wireguard`, …) | phase 3+ / community |
| `RouteHint.reject` hard drop | **done** (data-plane completion, item 3 — `gsp-core` drops the connection / datagram before routing: `gsp_listener_connections_total{result="sniffer_reject"}` / `gsp_datagrams_dropped_total{reason="sniffer_reject"}`, no reply) |
| Per-listener multiple distinct sniffers (only one name allowed today) | polish |
| TCP prefix binding beyond `freebind` (accepting a whole prefix on one socket — needs routing + `getsockname`, no cmsg), IPv4 non-local bind ergonomics | phase 3–6 |
| **Phase 4 — done**: external resolver HTTP + gRPC, `pool` + `target`, `on_error`, TTL LRU cache + `stale_ok` | **done** |
| **Phase 6 — done**: PROXY protocol v1/v2 (TCP) + v2-udp (first datagram); TPROXY transparent mode (TCP + UDP, v4 + v6) | **done** |
| UDP transparent `IPV6_TRANSPARENT` on a musl / non-glibc target, and `recvmmsg` batching for the transparent recv path | perf / portability pass |
| Resolver `sticky_key` — resolver-chosen affinity key; deferred (overlaps the request-keyed cache + `route_hint` + UDP affinity; needs a design for how a later request recovers the key) | later |
| `target` connections' connect / idle timeouts | **done** (data-plane completion, item 6 — `resolvers[].target_connect_timeout_ms` / `target_idle_timeout_sec`, defaults 300 ms / 90 s = `proxy::TARGET_*`, carried on `Routed::Target` via `Resolver::target_{connect,idle}_timeout`) |
| Build now needs `protoc` (gRPC codegen in `crates/gsp/build.rs`); CI installs `protobuf-compiler` | — |
| Resolver cache uses `std::sync::Mutex<LruCache>` — a brief lock on the routing path (not held across `.await`); like `Backend::observe`, deliberate | — |
| `resolvers:` live reload | **done** (data-plane completion, item 4 — `gsp_core::Resolvers` is `ArcSwap`-backed like `Sniffers`; the reload task rebuilds + `replace()`s the clients when `ResolverConfig` differs. Rebuild resets each `CachedResolver` LRU cache.) |
| `route_hint` per-conn cost adds a lock-free `ArcSwap<HashMap>` read when the listener opts in — recorded in the latency ledger | — |
| `sni` on a ClientHello split across TCP segments (single peek only; falls through) | **done** (data-plane completion, item 1 — `listener::peek_routing_bytes` re-peeks until the first TLS record is whole or the 250 ms budget expires) |
| Backend discovery adapters (DNS SRV, K8s, Consul) | **done** (phase 8) |
| k8s discovery via a watch-based informer (polling now) | perf pass |
| Live reload of `backend_sources` | **done** (data-plane completion) — `gsp_core::SourceManager` (discovery analogue of `ListenerManager`): one refresh task per pool `source` behind a private stop channel, `reconcile(&Snapshot)` on reload diffs the new `Snapshot::sources` map (pool → `SourceConfig`, newly echoed) and spawns / stops / restarts. `gsp` binary supplies a `SourceFactory` (`DiscoveryFactory`). A dropped `source` also `Discovery::forget`s the pool's cached set. |
| CIDR allow/deny filter chain (per-listener `allow` / `deny`) | **done** (phase 7 slice 1) |
| Rate limiting (per-listener token bucket, src_ip + /24 / /64) | **done** (phase 7 slice 2) |
| Global caps (`max_connections` / `max_udp_sessions` / `max_new_sessions_per_sec`) | **done** (phase 7 slice 3) |
| UDP first-packet gate (`first_packet_gate` on a UDP listener) | **done** (phase 7 slice 4) |
| Amplifier-checklist tests (`tests/amplification.rs`) | **done** (phase 7 slice 5) |
| ACL longest-prefix-match trie (`gsp_config::CidrSet`) | **done** (phase 7 slice 6) |
| Optional GeoIP country filter (`settings.geo_db` + per-listener `geo`) | **done** (phase 7 slice 7) |
| Parser fuzzing (`crates/gsp-config/fuzz/`, `make fuzz`, CI job) | **done** (phase 7 slice 8) |
| Per-source concurrent connection/session cap (`per_source`) | **done** (phase 7 slice 9) |
| NFR N1/N2 latency harness (`crates/gsp-bench`, `make bench`) | **done** (phase 7 slice 10) |
| NFR N1/N2-under-load, real separate-process concurrency ramp (`gsp-bench --mode concurrency`) | **done** (post-phase-9; up to ~20k TCP / ~5k UDP verified on this box) |
| NFR N3/N4/N5/N9 (aggregate throughput, full 500k/1M, HA) | need dedicated hardware, multiple hosts + a real load generator |
| Per-source cap: LRU eviction of idle sessions under pressure (refuse-when-full now) | polish |
| `panic = "abort"` in the release profile — fine, but be aware unwinding is off | — |

---

## Deferred-work plan (what to finish before the fleet phases, and what waits)

Analysis of every deferred item above + the phase-9 follow-ups + `docs/08`
"Later / optional", split by whether it belongs to **finishing the
single-instance proxy** or **after fleet management & controls (phases 10–12)**.
Suggested sequencing: one "data-plane completion" phase (list A + C) → one
"perf pass" phase (list B) → then phases 10–12. This matches the `docs/08`
milestone cut where v1.3 is additive and the data-plane contract is unchanged.

**Execution order within list A** (small self-contained wins first — keep `main`
continuously green and shippable; save the big cross-workspace change for a
focused unit):

1. ~~ClientHello fragmentation~~ — **done** (on `main`).
2. ~~Item 3 — `RouteHint.reject` hard drop~~ — **done** (on `main`).
3. ~~Item 6 — per-resolver `target` timeout knob~~ — **done** (on `main`).
4. ~~Item 5 — `weighted` balancer~~ — **done** (on `main`).
5. ~~**Item 2 — per-plugin sniffer config**~~ — **done** (on `main`), a two-slice unit:
   - ~~**A2**~~ — **done** (on `main`): `settings.sniffers.modules[].config`
     schema (+ `SnifferModulePin.config`, `validate_sniffers`), the widened
     guest ABI `sniff(in_ptr, in_len, cfg_ptr, cfg_len)` (host `alloc`s + writes
     both regions; `gsp_sniffer_abi::config()` on the guest side), `WasmSniffer`
     carries the config bytes and `scan()` fills them from the pin, all three
     first-party plugins take the two new args (a2s / minecraft ignore them),
     ADR 16a. Tests: `sniffer_loader::tests::{wasm_plugin_receives_its_config,
     build_sniffers_wires_module_config_through_the_scan}` +
     `gsp_config` config-parse/reject cases.
   - ~~**A3**~~ — **done** (on `main`): `regex-firstbytes` rebuilt around its
     `config` — a tiny `[key:NAME|] [@OFFSET ](hex:…|ascii:…){|…}` pattern
     language (no `regex` dep), `O(n)` and allocation-free, caps on offset /
     pattern count. Matches nothing without a `config`. Tests: 6 native in the
     crate + `sniffer_loader::tests::first_party_regex_firstbytes_matches_by_config`
     (`#[ignore]`d, real `.wasm` through the loader).
   **ABI decision (locked here)**: *widen `sniff`*, not an optional `configure`
   export. The sniffer ABI is a third-party contract that freezes at 1.0; there
   are zero external plugins today, so the clean break is at its cheapest now,
   and a uniform signature (config always passed, empty slice when none) beats a
   permanent "call `configure` if the module exports it" branch and a two-class
   plugin model. Extra blast radius is mechanical and in-tree.
6. **Item 4 — live reload of `resolvers:`** — **done** (on `main`).
   `gsp_core::Resolvers` became `ArcSwap`-backed (mirrors `Sniffers` slice 4);
   `Snapshot` carries `resolvers: Vec<ResolverConfig>` (echoed, for the diff);
   `reload::apply` rebuilds + `Resolvers::replace`s the clients only when
   `cfg.resolvers != prev.resolvers`. `build_resolvers` now returns the plain
   map (both `main.rs` — `Resolvers::from_map` — and the reload task use it).
   Test: `resolver::tests::replace_swaps_the_live_resolver_set`.
   **`backend_sources:` live reload** — **done** (data-plane completion,
   split-out slice). `gsp_core::SourceManager` (new `sources.rs`, the discovery
   analogue of `ListenerManager`): one `refresh_loop` task per pool `source`
   behind a private `watch<bool>` stop, `start_all` at boot, `reconcile(&Snapshot)`
   on reload. `Snapshot` now echoes `sources: HashMap<String, SourceConfig>`
   (pool → its dynamic source) for the diff; `reload::apply` calls
   `handle.reconcile_sources()` when `next.sources != prev.sources`. `gsp-core`
   stays HTTP-free — `trait SourceFactory` is supplied by the `gsp` binary
   (`discovery::DiscoveryFactory` → `build_one`). `Runtime::start_with_discovery`
   took a `Vec<Arc<dyn BackendSource>>`; now takes
   `Option<Arc<dyn SourceFactory>>` and owns the `SourceManager`
   (`stop_all` / `abort_all` wired into `shutdown_with_grace`). A dropped
   `source` also `Discovery::forget`s the pool. Tests:
   `sources::tests::reconcile_adds_restarts_and_removes_refresh_tasks`,
   `discovery::runtime_reconciles_sources_when_backend_sources_change`.

**List A is complete.** Then list C polish, then list B (perf pass).

**Verification (checked against the code at `34867bf`)**: none of these items
have been started, in any form — every one is still a `// later` comment or an
unimplemented branch. Three, though, already have a partial mechanism, so they
are cheaper than a fresh slice:
- **Item 1 (ClientHello fragmentation)** — **done** (see list A item 1). Was a
  one-shot `stream.peek()`; now a bounded re-peek loop keyed off the TLS record
  length.
- **Item 3 (`RouteHint.reject` hard drop)** — **done** (see list A item 3).
  `gsp-core` now drops a rejected connection / datagram before routing, on both
  the TCP and UDP paths; the `!reject` guards in `gsp-config` matching stayed as
  belt-and-braces for direct `route_for` callers.
- **Item 8 (`IPV6_TRANSPARENT` on musl)** — `set_ip_transparent` already calls
  `socket2` 0.6's `set_ip_transparent_v6` unconditionally (`net.rs:138`), with
  no `target_env` guard. This may already work on musl; start with a build +
  smoke test before assuming code is needed.

### Do NOW — to call the single-instance proxy "finished"

**A. Real feature / correctness gaps (a production deployment will hit these):**

1. ~~**ClientHello / first-bytes split across TCP segments**~~ — **DONE**
   (`listener::peek_routing_bytes` / `routing_bytes_complete`): when the first
   peeked byte is a TLS record and the first record is not yet whole, the TCP
   path re-peeks every 5 ms (`PEEK_POLL`) until it is complete or the 250 ms
   `PEEK_TIMEOUT` budget expires, then routes. Non-TLS first bytes keep the
   single-peek behaviour. Test:
   `tcp_forward::sni_matcher_reassembles_a_fragmented_client_hello`.
2. ~~**Per-plugin sniffer config**~~ — **DONE** (A2 + A3): `settings.sniffers.
   modules[].config` string + widened `sniff(in_ptr, in_len, cfg_ptr, cfg_len)`
   ABI (ADR 16a); `regex-firstbytes` rebuilt around it as a configurable bounded
   pattern matcher. See "Execution order" above.
3. ~~**`RouteHint.reject` → hard drop**~~ — **DONE**: `gsp-core` drops a
   rejected connection (`listener.rs`) / first datagram (`listener_udp.rs`,
   before the gate and the push hint) instead of falling through to `always`.
   `gsp_listener_connections_total{result="sniffer_reject"}` /
   `gsp_datagrams_dropped_total{reason="sniffer_reject"}`; UDP sends no reply.
   Tests: `sniff::tests::sniffer_reject_drops_the_tcp_connection` /
   `sniffer_reject_drops_the_udp_datagram_with_no_reply`.
4. ~~**Live reload of `resolvers:`**~~ — **DONE**: `Resolvers` is `ArcSwap`-backed;
   the reload task rebuilds + swaps the clients when `ResolverConfig` differs
   (LRU caches reset). `backend_sources:` live reload landed as its own
   split-out slice (`gsp_core::SourceManager` — see the list-A item 6 note).
5. ~~**`weighted` balancer**~~ — **DONE**: `balancer: weighted` + pool
   `weights: { "ip:port": N }` (weight `>= 1`, default 1, `weighted`-only),
   weighted round-robin over the healthy set in `Pool::acquire_for` (one atomic
   tick into the cumulative-weight line). Works with a discovered `source:`.
   Tests: `pool::tests::weighted_distributes_new_sessions_by_weight` /
   `weighted_falls_through_when_the_heavier_backend_is_full` /
   `weighted_without_a_weights_map_is_plain_round_robin` +
   `gsp_config::tests::parses_weighted_pool_and_rejects_bad_weights`.
   (`first_available` can wait.)
6. ~~**Per-resolver `target` connect/idle timeout knob**~~ — **DONE**:
   `resolvers[].target_connect_timeout_ms` / `target_idle_timeout_sec` (defaults
   300 ms / 90 s), carried on `Routed::Target` via new `Resolver` trait methods
   (default = `proxy::TARGET_*`), forwarded by `CachedResolver`, set by
   `HttpResolver` / `GrpcResolver`. Test:
   `resolver::tests::resolver_target_carries_per_resolver_timeouts` +
   `gsp_config::tests::parses_resolver_target_timeouts_and_rejects_zero`.

**B. Dedicated performance pass (its own phase, before v1.x — the NFRs are the
project's north star):**

7. **`splice()` zero-copy TCP**, **`recvmmsg`/`sendmmsg` UDP batching** (incl.
   the transparent recv path), **UDP idle expiry via a timing wheel**. All
   already scoped as drop-in replacements behind the same fn. These gate
   N3/N4/N5 throughput.
8. **`IPV6_TRANSPARENT` on musl / non-glibc** — pull in here if shipping Alpine
   containers.

**C. Cheap polish / docs (fold into A or B):**

9. ~~**epoch-ticker thread in `docs/02`** threading-model list~~ — **done** (on
   `main`): noted as a dedicated non-tokio OS thread (`gsp-sniffer-epoch`),
   absent without `settings.sniffers`.
10. ~~**`GET /sessions`** introspection~~ — **done** (on `main`):
    `ConnTracker` gained an `id → {proto, listener, peer, local, pool, backend,
    since}` registry alongside its `watch<usize>` counter (a brief `Mutex` at
    `track` / drop / `set_target` — once per connection, never per byte).
    TCP fills `pool`/`backend` via `ConnGuard::set_target` from `proxy::handle_tcp`
    once the backend is picked; UDP fills them at `Session` creation. Exposed as
    `RuntimeHandle::sessions()` and the filterable plaintext admin endpoint
    `GET /sessions[?listener=&pool=&proto=&src=]`. Tests:
    `drain::tests::sessions_reflects_live_entries_and_set_target`,
    `tcp_forward::sessions_registry_lists_a_live_connection_with_its_pool_and_backend`,
    `udp_forward::sessions_registry_lists_a_live_udp_session_with_its_pool_and_backend`.
    Follow-up slice: `admin.rs` grew `#[cfg(test)] mod tests` (the first
    HTTP-level admin coverage) — `serve` was split into `router(state) -> Router`
    + the bind loop so a test can mount the real route table on an ephemeral
    listener and drive it with `reqwest` (`gsp` is binary-only, so this lives
    in-module, not in `crates/gsp/tests/`). Covers `healthz` / `readyz` /
    `pools` / `config` and `sessions` incl. every query filter (match + exclude).
    `PrometheusBuilder::build_recorder().handle()` gives a non-global handle so
    the tests don't fight over the global recorder.
11. **Reload debounce widening** (only coalesces within one 200 ms window) — low
    priority, low cost.
12. **LRU eviction for the UDP sticky table and the per-source cap**
    (refuse / wholesale-clear when full now) — acceptable defaults; do only if
    load testing shows them biting.

### WAIT until after fleet management & controls (phases 10–12)

- **NFR N9 (HA)** — HA is anycast / L4-LB in front of a fleet; can't be
  validated before the fleet exists.
- **NFR N3 / N4 / N5 (aggregate throughput, full 500k / 1M conns)** — need
  multiple hosts + a real load generator; done on the same hardware used to
  test the fleet. The concurrency-ramp harness already went as far as one box
  allows.
- **Resolver `sticky_key`** — deferral is explicitly pending a design for how a
  later request recovers the key; that overlaps the control-plane intent/routing
  model. Do it with phase 11.
- **k8s discovery watch informer** (polling now) — a convergence-speed
  optimization that belongs with the discovery/scaling rework the fleet phases
  touch.
- **Retire the UDP sticky table via `consistent_hash`** — pure polish, no
  user-visible gap.
- **TCP whole-prefix bind** (beyond `freebind`) — niche, no demand signal;
  revisit if a user asks.
- **More sniffers (`quic`, `wireguard`, …)** — community / plugin ecosystem
  work, genuinely parallel, never blocking core completion.
- **Multiple distinct sniffers per listener** — polish; wait for a real use case.
- **TLS/DTLS termination, QUIC-CID session keying, eBPF/XDP pre-filter** —
  already parked under `docs/08` "Later / optional"; each is its own project.
- **`serde_yaml` deprecation** — monitor only; act if it actually breaks.

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
path also does a `MSG_PEEK` (into a `peek_len()`-sized `Vec` — `PEEK_MAX` =
4096 for `sni` / `sniffer`) with a 250 ms total budget, and clones the
listener's `Arc<ListenerConfig>` into the per-conn task. The peek is a single
syscall unless the first byte is a TLS record (`0x16`) and the first record is
not yet whole, in which case it re-peeks every 5 ms (`PEEK_POLL`) until the
record is complete or the budget expires — bounded, no allocation per retry,
still no lock or extra task. A `sniffer` route would also run
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

**Backend discovery** (`backend_sources`, phase 8): control-plane only. One
`refresh_loop` task per dynamic source wakes on its `refresh_interval_sec`,
does one `fetch()` (DNS SRV query / Consul or k8s HTTP GET), and on a change
`Discovery::store` (one `Mutex` + sort/dedup) + `reload.notify_one()`. The
snapshot rebuild reads `Discovery::get` (one `Mutex` + `Vec` clone) per pool
with a `source`. **Zero data-path cost** — the discovered set only affects a
pool's backend `Vec` at rebuild time, exactly like the backend overlay.

**Sniffer plugins** (phase 9 slice 3, `WasmSniffer`): zero cost on any
listener without a `sniffer:` route (the registry lookup was already there
since slice 1 and costs one `HashMap::get`). On a listener that has one, each
call now does one `Store::new` + `Instance::new` (fresh per call — no state
carried between connections) + a `memory.write` of the peeked bytes + one
guest function call + a `memory.data` read-back and decode — all synchronous,
on the same spawned per-conn task (TCP) / `open_session` (UDP) that already
pays for the peek, never on the accept loop. Bounded by
`settings.sniffers.call_timeout_ms` (epoch interruption traps a runaway call)
and `max_memory_bytes`. **Benchmarked (slice 6)**: p50 8–10 µs, p99 12–26 µs
for the three first-party plugins (real compiled `.wasm`, release build,
loopback on this box) — comfortably inside NFR N1's 500 µs budget, so the
fresh-`Store`-per-call design needed none of the `InstancePre` /
pooling-allocator / warm-instance-per-worker fallbacks that were held in
reserve. See `docs/07` "Sniffer plugin sandbox guarantees" for the full
table and reproduction command.

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
the pool (or, for a resolver `target`, `resolvers[].proxy_protocol`) opts in;
`none` pays nothing. A `target` header costs the same as a pooled one — one
`Vec` + one `write_all` before the pump. No lock, no task, nothing per byte.

**Filter chain — CIDR allow/deny** (`allow` / `deny` on a listener): per new TCP
connection / new UDP session only, a bounded bit-walk of the `deny` radix trie
and (when `allow` is non-empty) the `allow` trie — at most 32 (v4) / 128 (v6)
node hops, early-exiting on the first covering prefix; independent of list size,
no alloc, no lock, no task. Tries are built once per listener spawn (in
`validate()`). TCP runs it before the task spawn; UDP runs it only for datagrams
that don't hit an established session, so the steady-state per-datagram path is
unchanged. Listeners with neither list pay one `Acl::is_empty` check.

**GeoIP filter** (`geo` on a listener): per new TCP connection / new UDP session
only — one MaxMind tree lookup (bounded bit-walk of an mmap-free in-memory
buffer) plus a small `Vec` scan of the listener's country codes. No lock, no
task, no per-connection alloc beyond a transient `String` for the ISO code.
Listeners without `geo` pay one `Option::is_some` check. The `GeoDb` is opened
once at startup.

**Rate limiting** (`rate_limit` on a listener): per new TCP connection / new UDP
session only — one `Mutex<HashMap>` lock (not held across `.await`, like
`Backend::observe`), one or two `HashMap` entry lookups, f64 refill arithmetic,
no task, no per-connection heap alloc (entries are reused; prune is amortised).
UDP runs it only for datagrams that miss an established session, so the
steady-state per-datagram path is untouched. Listeners without `rate_limit` pay a
single `is_enabled()` bool check (`permit` short-circuits before locking).

**Per-source concurrent cap** (`per_source` on a listener): per new TCP
connection / new UDP session only — one `Mutex<HashMap>` lock (no `.await`), one
or two `HashMap` get + entry-bump, no alloc beyond a possible bucket insert, no
task. One `SourceGuard` created/dropped per connection / session. Unconfigured ⇒
one `is_enabled()` check.

**Global caps** (`settings.limits`): per new TCP connection / new UDP session
only — one or two relaxed-ish atomic ops (`fetch_add` + maybe `fetch_sub`) and,
if `max_new_sessions_per_sec` is set, one short `Mutex<NewRate>` lock (no
`.await`). One `LimitGuard` created/dropped per connection / session, never per
byte / datagram. Uncapped ⇒ a single `is_enabled()` check.

**UDP first-packet gate** (`first_packet_gate`): per new UDP session only, inside
`open_session` — the sniff hint is already computed for routing; the gate adds a
short linear scan of the route list for a matching `FirstBytes` matcher (only
until routing itself runs). No lock, no alloc, no task; steady-state datagrams
never touch it. Listeners without the flag pay nothing.

**UDP passive health (ICMP port-unreachable)**: one extra `Arc<Backend>` clone
per UDP session at `open_session` (moved into `Session` + the reply task). On a
`send`/`recv` error only, one `io::Error::kind()` compare and — for
`ConnectionRefused` — one `Backend::observe(false)` (the same short streak
`Mutex` the active checker and TCP passive path already take). Nothing on the
steady-state datagram path.

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

`RouteHint.reject` → hard drop landed later (data-plane completion, item 3):
`gsp-core` drops a rejected connection / datagram before routing rather than
falling through to `always`. The loader itself is Phase 9.

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
plugin) = phase 9; `weighted` balancer shipped later (data-plane completion);
`first_available` balancer = later.

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
`handle_tcp_target(stream, addr, connect_timeout, idle_timeout)` (no guard; the
timeouts default to `TARGET_*` and are per-resolver configurable since the
data-plane-completion item 6 slice). `listener_udp::open_session` builds `(backend, Option<BackendGuard>,
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
Resolver config was startup-only here; it reloads live since the data-plane
completion pass (`Resolvers` is `ArcSwap`-backed, rebuilt on a `resolvers:`
change — see the deferred table).

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
Resolver `target` connections read the header form from
`resolvers[].proxy_protocol` (pre-phase-8 cleanup); a push-hint / direct-config
target is `ProxyProtocol::None`.

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
| `crates/gsp-core/src/proxy.rs` | `handle_tcp` (pool; writes the pool's PROXY protocol header before the pump) / `handle_tcp_target` (resolver `target`, no guard; writes the resolver's `proxy_protocol` header) → `connect_backend` + `pump` (`copy_with_idle` both ways). |
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
