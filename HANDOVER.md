# HANDOVER

State of the work, decisions already made, and how to pick it up.
Last updated: 2026-09-03 (after roadmap phase 2).

---

## TL;DR

- **Planning docs** (`docs/00`–`09`) are complete and in English. They are the design
  source of truth.
- **Code**: Cargo workspace, roadmap **phases 0–2 complete**. The proxy forwards
  **TCP and UDP** end to end with health checks (`tcp_connect` + `udp_probe`), two
  balancers, per-backend caps, worker-local UDP session tables with `src_ip`
  affinity, and hot reload.
- **Next**: roadmap **phase 3 — routing intelligence** (route rule list + matchers;
  see `docs/03` and `docs/08`). Note below.
- **Build/verify**: `make check` (fmt + clippy `-D warnings` + 22 tests, all green).
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
- **Static listener → pool** routing (one pool per listener).
- **Balancers**: `round_robin`, `least_conn` (counts UDP sessions too).
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
  health-check / cap changes are **live**; changes to a listener's bind, protocol, or
  pool mapping are **not** applied live (logged as a warning — full listener
  reconfiguration is phase 5).
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

### Tests (22, all green)

- `gsp-config` (11): schema parsing + validation rejections, incl. UDP listener +
  default affinity, affinity-on-TCP rejection, `udp_probe` parsing, `udp_probe`
  without `send_hex` rejection.
- `gsp-core` unit (7): round-robin cycling, least-conn preference, capacity rejection,
  unhealthy-skip, all-unhealthy error, `rise`/`fall` thresholds, reload health
  carry-over.
- `gsp-core/tests/tcp_forward.rs` (2): end-to-end client→proxy→backend byte forwarding;
  "routes around a dead backend".
- `gsp-core/tests/udp_forward.rs` (2): end-to-end UDP datagram forwarding + session
  reuse / affinity (same client → same backend); idle-timeout eviction frees the
  per-backend slot.

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
| Balancers | `round_robin` (atomic index + `rotate_left`), `least_conn` (sort healthy by active). |
| UDP | Worker-local session table (no global lock), `connect(2)` socket + reply task per session, per-worker sticky affinity table (hard cap, wholesale clear), 1 s idle sweep. `recvmmsg`/`sendmmsg`, timing wheel, `consistent_hash` deferred. See ADR 9. |
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
| `consistent_hash` balancer | phase 3 |
| UDP ICMP port-unreachable as an explicit passive health signal (currently just ends the reply pump; the idle sweep reaps) | phase 5–7 |
| Listener add / remove / rebind at runtime (needs restart today) | phase 5 |
| Tracked connection drain with a grace period on shutdown | phase 5 |
| Full CRUD admin API (add/remove backend, set `draining`/`disabled` state) | phase 5 |
| `draining` / `disabled` backend states (only `healthy`/`unhealthy` exist) | phase 5 |
| Reload debounce only coalesces within one 200 ms window; wider-spaced events cause separate (idempotent) reloads | polish, low priority |
| Routing matchers (`sni`, `first-bytes`, `dst`, `port`, `client-cidr`, `external`) | phase 3–4 |
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

## Phase 3 — routing intelligence (the next slice)

Goal: replace "one pool per listener" with a priority-ordered route rule list.
Design reference: `docs/03-routing.md` and `docs/08` phase 3.

Rough scope: `routes: [{ match, action }]` on the listener; matchers `always`,
`port`, `client-cidr`, `first-bytes` (TCP peek / first UDP datagram); `action.pool`
selects the pool. `consistent_hash` balancer fits here. Keep the agnostic core:
sniffers/regex parsers are optional plugins, not in the forwarding path.

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
| `crates/gsp-core/src/pool.rs` | `Pool` (balancer + `rr` index, `acquire` / `acquire_addr`), `Backend` (health/active/streaks/`check_kind`), `BackendGuard` (RAII slot + passive health), `PickError`. |
| `crates/gsp-core/src/listener.rs` | `run_tcp_listener`: accept loop, per-conn spawn, connection metrics + logs. |
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
