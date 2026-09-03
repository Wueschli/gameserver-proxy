# HANDOVER

State of the work, decisions already made, and how to pick it up.
Last updated: 2026-09-03 (after roadmap phase 2 + phase 3 routing slices 1–3).

---

## TL;DR

- **Planning docs** (`docs/00`–`09`) are complete and in English. They are the design
  source of truth.
- **Code**: Cargo workspace, roadmap **phases 0–2 complete**, **phase 3 slices
  1–3 landed** (per-listener route rule list + `first_bytes` prefix matcher +
  `consistent_hash` balancer). The proxy forwards **TCP and UDP** end to end
  with health checks (`tcp_connect` + `udp_probe`), three balancers, per-backend
  caps, worker-local UDP session tables with `src_ip` affinity, hot reload, and
  address- + first-bytes routing.
- **Next**: phase 3 continued — `first_bytes` `regex` / `length` variants, then
  `dst` / `sni` / sniffer plugins. See `docs/03` and `docs/08`. Note below.
- **Build/verify**: `make check` (fmt + clippy `-D warnings` + 38 tests, all green).
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
  client `SocketAddr`; one upstream socket `connect(2)`-ed to the chosen backend per
  session plus a reply-pump task; `src_ip` / `src_ip_port` **backend affinity** via a
  per-worker sticky table; **idle-timeout eviction** (1 s sweep, `idle_timeout_sec`
  from the pool, read once at session creation) that releases the `BackendGuard`;
  **amplification guard** — the proxy never sends to a client without an established
  session.
- **Per-listener route rule list** (`listeners[].routes`, priority-ordered, first
  match wins; `action: { pool }`). Matchers: `always`; `client_cidr` (source IP,
  hand-rolled CIDR in `gsp-config` — no `ipnet` dep); `port` (destination port
  from the accepting socket, single or `"lo-hi"` range); `first_bytes` (prefix
  of the first bytes, `hex:` / `ascii:`, ≤ `PEEK_MAX` = 512 B). A bare `pool:`
  is normalised to one `always` route. No match → connection/datagram dropped
  (`gsp_listener_connections_total{result="no_route"}` /
  `gsp_datagrams_dropped_total{reason="no_route"}`). TCP `MSG_PEEK`s
  `ListenerConfig::peek_len()` bytes (250 ms budget, `PEEK_TIMEOUT`) in the
  spawned per-conn task only when a route needs them; UDP routes on the first
  datagram it already holds. A silent TCP client routes as if it sent nothing.
- **Balancers**: `round_robin`, `least_conn` (counts UDP sessions too),
  `consistent_hash` (rendezvous/HRW hash of the client key — pool `hash_on:
  src_ip | src_ip_port` — over the healthy backends; `acquire()` with no client
  key falls back to round-robin). TCP passes `peer`; UDP passes the client addr
  on the non-sticky path.
- **Per-connection pump**: buffered bidirectional copy, connect timeout, per-direction
  idle timeout, half-close propagation, `TCP_NODELAY`.
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
  health-check / cap changes are **live**; changes to a listener's bind, protocol,
  routes, or affinity are **not** applied live (logged as a warning — full listener
  reconfiguration is phase 5). Route rules are captured per listener task at
  startup; only the *pool contents* they resolve to are read live from the
  snapshot.
- **Admin API** (`settings.admin.listen`, default `127.0.0.1:9900`):
  `/healthz`, `/readyz`, `/metrics` (Prometheus), `/pools` (per-backend health +
  active count).
- **Metrics**: see `crates/gsp-core/src/metrics_defs.rs`. Connections, bytes,
  duration, backend connect errors, `gsp_pool_backends`, `gsp_healthcheck_total`,
  `gsp_lb_selections_total`, `gsp_config_reload_total`, `gsp_config_version`, and
  for UDP: `gsp_active_udp_sessions{listener}`, `gsp_packets_total{listener,dir}`,
  `gsp_datagrams_dropped_total{listener,reason}`.
- **Graceful stop** on SIGINT/SIGTERM: listeners and the health checker stop; in-flight
  connections are detached (tracked drain with a grace period is phase 5).

### Tests (38, all green)

- `gsp-config` (22): schema parsing + validation rejections, incl. UDP listener +
  default affinity, affinity-on-TCP rejection, `udp_probe` parsing, `udp_probe`
  without `send_hex` rejection, `consistent_hash` parsing + default/explicit
  `hash_on`, `hash_on`-without-`consistent_hash` rejection; **routing**: bare
  `pool` → one `always` route, route-list first-match (`client_cidr` / `port` /
  `always`), `pool`+`routes` rejection, unknown-pool-in-route rejection,
  `always`-with-fields rejection, bad-CIDR / reversed-range / empty-`cidrs`
  rejection, `Cidr::contains` v4 + v6, `first_bytes` prefix match + `peek_len()`,
  bad `first_bytes` specs (no `hex:`/`ascii:` tag, missing/empty prefix, prefix
  on `always`).
- `gsp-core` unit (9): round-robin cycling, least-conn preference, capacity
  rejection, unhealthy-skip, all-unhealthy error, `rise`/`fall` thresholds,
  reload health carry-over; `consistent_hash` stability + spread (`src_ip`
  ignores port), and "only the lost backend's share moves" when a backend fails.
- `gsp-core/tests/tcp_forward.rs` (4): end-to-end client→proxy→backend byte
  forwarding; "routes around a dead backend"; "first matching route selects the
  pool" (`client_cidr` hit vs. fall-through to `always`); "consistent_hash pins
  a client to one backend".
- `gsp-core/tests/udp_forward.rs` (3): end-to-end UDP datagram forwarding + session
  reuse / affinity (same client → same backend); idle-timeout eviction frees the
  per-backend slot; `first_bytes` prefix routes to its pool vs. `always`.

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
| UDP client-IP, TPROXY, PROXY protocol, discovery adapters, sniffers, external resolver | **designed in `docs/`, not yet built.** |
| Deps kept out of `gsp-core` | `axum`, `clap`, `notify` live in the `gsp` binary only. |

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
| Listener add / remove / rebind at runtime (needs restart today) | phase 5 |
| Tracked connection drain with a grace period on shutdown | phase 5 |
| Full CRUD admin API (add/remove backend, set `draining`/`disabled` state) | phase 5 |
| `draining` / `disabled` backend states (only `healthy`/`unhealthy` exist) | phase 5 |
| Reload debounce only coalesces within one 200 ms window; wider-spaced events cause separate (idempotent) reloads | polish, low priority |
| Routing matchers `always` / `client_cidr` / `port` / `first_bytes` (prefix); `consistent_hash` balancer | **done** (phase 3 slices 1–3) |
| `first_bytes` `regex` / `length` / `sniffer` variants | phase 3 |
| Routing matchers `sni`, `dst`, `external` | phase 3–4 |
| Backend discovery adapters (DNS SRV, K8s, Consul) | phase 8 |
| Rate limiting, ACLs, geo, first-packet gate | phase 7 |
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
entry, `u16` range check per `port` entry, `starts_with` per `first_bytes`
entry. When (and only when) a route uses `first_bytes`, the TCP path also does
one `MSG_PEEK` (into a `peek_len()`-sized `Vec`, ≤ 512 B) with a 250 ms timeout,
and clones the listener's `Arc<ListenerConfig>` into the per-conn task. No lock,
no task spawn beyond the existing per-conn one, nothing on the per-byte /
per-datagram path. TCP route resolution now happens inside the spawned task, so
the accept loop no longer loads the snapshot.

`consistent_hash` selection is `O(healthy)` — one `Vec<&Backend>` of the healthy
set (already built for every balancer) plus a `sort_by_key` with one
`DefaultHasher` (SipHash of client IP [+ port] and backend addr) per backend. No
allocation beyond that `Vec`, no lock. `least_conn` already sorts the same `Vec`,
so this is the same order of work.

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

### Slice 2 — `first_bytes` prefix matcher (done)

`match: { type: first_bytes, prefix: "hex:ffffffff" | "ascii:GET " }` — a
non-empty prefix (≤ `PEEK_MAX` = 512 B) of the connection's first bytes.
`Matcher::FirstBytes(Vec<u8>)`, matched with `starts_with`. `MatchContext`
carries `first_bytes: &[u8]` (empty when nothing was peeked / sent).
`ListenerConfig::peek_len()` = max prefix length over the route list, 0 when no
byte matcher — the TCP path skips the peek entirely in that case. TCP peek:
`TcpStream::peek` in the per-conn task, `PEEK_TIMEOUT` = 250 ms; a silent client
routes as if it sent nothing. UDP: routes on the first datagram, already in hand
in `open_session`. `parse_byte_spec` handles the `hex:` / `ascii:` tags.

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

### Slice 4 — next

`first_bytes` `regex` (precompiled, bounded `N`; needs the `regex` crate — the
dep decision was deferred, ideally taken with the sniffer-plugin API) and
`length` (datagram-length range, UDP-centric) variants. Then `sni` peek and the
in-process sniffer plugin API (`sni`, `minecraft`, `a2s`). Keep the agnostic
core: sniffers/regex parsers are optional plugins, never in the forwarding path.

### Do NOT

- Add QUIC connection-ID awareness (later stage).
- Add cross-instance session handover.
- Start the external resolver (phase 4) while doing routing matchers.

---

## Codebase map (quick reference)

| File | Responsibility |
|------|----------------|
| `crates/gsp-config/src/lib.rs` | Raw YAML types, `validate()`, resolved `Config`/`PoolConfig`/`ListenerConfig`/`HealthCheck`. All schema rules here. |
| `crates/gsp-core/src/snapshot.rs` | `Snapshot { listeners, pools }`; `build(cfg, prev)` carries health over. |
| `crates/gsp-core/src/pool.rs` | `Pool` (balancer + `rr` index + `hash_on`, `acquire` / `acquire_for` / `acquire_addr`, `hrw_score`), `Backend` (health/active/streaks/`check_kind`), `BackendGuard` (RAII slot + passive health), `PickError`. |
| `crates/gsp-core/src/listener.rs` | `run_tcp_listener`: accept loop; per-conn task does first-bytes peek + route match + pool lookup, then metrics + logs. |
| `crates/gsp-core/src/listener_udp.rs` | `run_udp_listener`: per-worker recv loop, session table, sticky affinity, idle sweep, per-session upstream socket + reply pump. |
| `crates/gsp-core/src/proxy.rs` | `handle_tcp`: acquire backend, connect, `copy_with_idle` both ways. |
| `crates/gsp-core/src/health.rs` | `run`: 500 ms sweep, probes due backends (`tcp_connect` / `udp_probe`), updates health + gauges. |
| `crates/gsp-core/src/runtime.rs` | `Runtime::start` spawns listener + health tasks; `RuntimeHandle` (`current`/`store`/`ready`). |
| `crates/gsp-core/src/net.rs` | `bind_reuseport_tcp`. |
| `crates/gsp-core/src/metrics_defs.rs` | Every metric name. |
| `crates/gsp/src/main.rs` | CLI (`--config`, `--check`), tracing init, runtime bring-up, shutdown. |
| `crates/gsp/src/admin.rs` | axum router: `/healthz` `/readyz` `/metrics` `/pools`. |
| `crates/gsp/src/reload.rs` | `SIGHUP` + `notify` file watch → debounce → `apply` (validate, build, store). |

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
- CI: `.github/workflows/ci.yml` runs `cargo fmt --check`, `clippy --all-targets
  --all-features`, `cargo test --all` on push/PR.
- `git push` is not possible from this environment. To enable:
  `git remote set-url origin git@github.com:Wueschli/gameserver-proxy.git` (SSH), or
  configure a credential helper / PAT for HTTPS.
- No `LICENSE` decision has been *made* by the user, but `Cargo.toml` declares
  `MIT OR Apache-2.0` and `LICENSE-MIT` / `LICENSE-APACHE` are included to match.
  Confirm this is intended.
