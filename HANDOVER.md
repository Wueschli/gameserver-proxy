# HANDOVER

State of the work, decisions already made, and how to pick it up.
Last updated: 2026-09-03 (after roadmap phase 1).

---

## TL;DR

- **Planning docs** (`docs/00`–`09`) are complete and in English. They are the design
  source of truth.
- **Code**: Cargo workspace, roadmap **phases 0 and 1 complete**. The proxy forwards
  TCP end to end with health checks, two balancers, per-backend caps, and hot reload.
- **Next**: roadmap **phase 2 — UDP** (plan below).
- **Build/verify**: `make check` (fmt + clippy `-D warnings` + 17 tests, all green).
- **Infra**: git repo, remote `github.com/Wueschli/gameserver-proxy`, branch `main`.
  Local is **ahead of `origin/main` and unpushed** — pushing is blocked in this
  environment (no credentials; the HTTPS credential helper points at a nonexistent
  Windows path). Push from a machine with credentials, or switch the remote to SSH.

---

## What works today

Run `cargo run -p gsp -- --config config.example.yaml` and you get:

- **TCP listeners**, one accept task per CPU core per listener, each on its own
  `SO_REUSEPORT` socket.
- **Static listener → pool** routing (one pool per listener).
- **Balancers**: `round_robin`, `least_conn`.
- **Per-connection pump**: buffered bidirectional copy, connect timeout, per-direction
  idle timeout, half-close propagation, `TCP_NODELAY`.
- **Backend health**: active `tcp_connect` probes per pool (`interval`, `timeout`,
  `rise`, `fall`); passive marking on connect failure/timeout; unhealthy backends are
  excluded from selection; `PickError` when none are available.
- **Per-backend session cap** (`per_backend.max_sessions`).
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
  `gsp_lb_selections_total`, `gsp_config_reload_total`, `gsp_config_version`.
- **Graceful stop** on SIGINT/SIGTERM: listeners and the health checker stop; in-flight
  connections are detached (tracked drain with a grace period is phase 5).

### Tests (17, all green)

- `gsp-config` (8): schema parsing + validation rejections.
- `gsp-core` unit (7): round-robin cycling, least-conn preference, capacity rejection,
  unhealthy-skip, all-unhealthy error, `rise`/`fall` thresholds, reload health
  carry-over.
- `gsp-core/tests/tcp_forward.rs` (2): end-to-end client→proxy→backend byte forwarding;
  "routes around a dead backend".

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
| UDP client-IP, TPROXY, PROXY protocol, discovery adapters, sniffers, external resolver | **designed in `docs/`, not yet built.** |
| Deps kept out of `gsp-core` | `axum`, `clap`, `notify` live in the `gsp` binary only. |

---

## Known limitations / deferred (with the phase that addresses them)

| Item | Deferred to |
|------|-------------|
| `splice()` zero-copy TCP fast path (buffered copy for now, behind the same fn) | perf pass, any time |
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

Per-connection cost today: 1 `Pool::acquire` (lock-free reads + one atomic add),
1 backend `TcpStream::connect`, 1 spawned task for the pump. No per-byte allocation
beyond the two 32 KB direction buffers. Nothing per-connection touches a lock.
**If you add a per-connection or per-datagram task, hop, or allocation, record it
here.**

---

## Phase 2 — UDP (the next slice)

Goal: `protocol: udp` listeners forwarding datagrams to the same pool/health
machinery, with per-client sessions. Design reference: `docs/04-transport-and-client-ip.md`
(UDP section) and `docs/02-architecture.md` (component 4).

### Scope

1. **Config**: allow `protocol: udp` in `gsp-config` (remove the current "UDP not
   implemented" rejection). Add `idle_timeout_sec` semantics for UDP sessions (reuse
   the pool field). Add `affinity: { hash_on: src_ip }` (default on for UDP).
2. **UDP listener** (`gsp-core/src/listener_udp.rs` or fold into `listener.rs`):
   - `SO_REUSEPORT` UDP socket per worker; `recvmmsg` batch receive.
   - **Thread-local session table** `HashMap<(SrcAddr) or 4-tuple, Session>` — no
     locks (sessions pinned to the accepting worker).
   - Per session: one upstream socket `connect(2)`-ed to the chosen backend, so
     replies return without a table lookup; `sendmmsg` toward the backend.
   - **Idle timeout** via a timing wheel (or a simpler `tokio::time` per-session
     sleep for the first cut — note the tradeoff).
   - Session affinity: consult a sticky map keyed by `hash_on` before `Pool::acquire`.
3. **Health**: add a `udp_probe` health-check type (send bytes / expect prefix) to
   `gsp-config` + `health.rs`. Keep `tcp_connect` working.
4. **Backend integration**: `Pool::acquire` already returns a `BackendGuard`; hold one
   per UDP session, release on idle-timeout/close. `least_conn` then counts sessions.
5. **Metrics**: `gsp_active_udp_sessions{listener}`, `gsp_packets_total`,
   `gsp_datagrams_dropped_total{reason}`. Add names to `metrics_defs.rs` + `docs/06`.
6. **Tests**: e2e UDP echo through the proxy; session reuse (same 4-tuple → same
   backend); idle-timeout eviction; unhealthy backend not selected for new sessions.

### Watch out for

- Sessions must be **worker-local** — do not reach for a global `Mutex<HashMap>`.
- Reply path: bind the per-session upstream socket, don't demux manually.
- Don't reassemble fragments; don't answer datagrams that haven't established a session
  (amplification guard — full version is phase 7, but "no unsolicited reply" is cheap
  and belongs here).
- Keep the `Snapshot`/reload contract intact: a reload rebuilds pools; existing UDP
  sessions keep running against their old `Arc<Backend>` (health carries over by
  address, active counts do not — same as TCP).

### Do NOT

- Add QUIC connection-ID awareness (later stage).
- Add cross-instance session handover.
- Change the TCP path while doing this.

---

## Codebase map (quick reference)

| File | Responsibility |
|------|----------------|
| `crates/gsp-config/src/lib.rs` | Raw YAML types, `validate()`, resolved `Config`/`PoolConfig`/`ListenerConfig`/`HealthCheck`. All schema rules here. |
| `crates/gsp-core/src/snapshot.rs` | `Snapshot { listeners, pools }`; `build(cfg, prev)` carries health over. |
| `crates/gsp-core/src/pool.rs` | `Pool` (balancer + `rr` index), `Backend` (health/active/streaks), `BackendGuard` (RAII slot + passive health), `PickError`. |
| `crates/gsp-core/src/listener.rs` | `run_tcp_listener`: accept loop, per-conn spawn, connection metrics + logs. |
| `crates/gsp-core/src/proxy.rs` | `handle_tcp`: acquire backend, connect, `copy_with_idle` both ways. |
| `crates/gsp-core/src/health.rs` | `run`: 500 ms sweep, probes due backends, updates health + gauges. |
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
