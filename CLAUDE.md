# CLAUDE.md

Guidance for Claude Code (and any AI agent) working in this repository.
Humans: this doubles as the contributor quick-reference.

---

## Start here (every session)

1. Read [`HANDOVER.md`](HANDOVER.md) — current state, locked decisions, and the plan
   for the next slice.
2. Run `make check` once to confirm a green baseline (fmt + clippy `-D warnings` +
   tests) before changing anything.
3. Skim the relevant `docs/` chapter before any structural change (design is the
   source of truth there, not the code).

---

## What this is

A **game-agnostic game server reverse proxy**: one entry point in front of arbitrary
game servers, forwarding TCP (and later UDP) transparently to backend pools without
knowing the game protocol. Written in Rust on `tokio`.

The full design lives in [`docs/`](docs/) — read [`docs/00-overview.md`](docs/00-overview.md)
and [`docs/02-architecture.md`](docs/02-architecture.md) before making structural
changes. Current progress and the next step are in [`HANDOVER.md`](HANDOVER.md).

---

## Repository layout

```
Cargo.toml                  workspace (resolver 2, edition 2021)
rust-toolchain.toml         pins stable
config.example.yaml         reduced v0 config schema
Makefile                    make check / test / run / fmt / lint
docs/                       the plan (00–09) — source of truth for design
crates/
  gsp-config/               YAML config: raw types, validation, resolved `Config`
  gsp-core/                 data plane
    snapshot.rs            immutable `Snapshot` (listeners + pools) behind ArcSwap
    pool.rs                 `Pool`, `Backend` (health + active count), `BackendGuard`
    listener.rs             TCP accept loop (one task per worker, SO_REUSEPORT)
    listener_udp.rs         UDP recv loop + worker-local session table + reply pump
    proxy.rs                per-connection byte pump
    sniff.rs                static sniffer plugins (sni / minecraft / a2s) — the ONLY place for game protocol parsing
    health.rs               active health-check sweep task (tcp_connect + udp_probe)
    runtime.rs              owns listener + health tasks, holds the ArcSwap
    net.rs                  socket helpers (SO_REUSEPORT bind, IP_PKTINFO, IP_FREEBIND)
    metrics_defs.rs         canonical metric names — ALL metric names live here
    util.rs                 tiny helpers (monotonic now_ms)
  gsp/                       binary
    main.rs                 CLI, tracing, runtime bring-up, shutdown
    admin.rs                axum admin API: /healthz /readyz /metrics /pools
    reload.rs               SIGHUP + file-watch → rebuild snapshot → atomic swap
```

Dependency direction: `gsp` → `gsp-core` → `gsp-config`. Never reverse it.

---

## Commands

The toolchain is installed via `rustup`. If `cargo` is not on `PATH`:

```sh
export PATH="$HOME/.cargo/bin:$PATH"     # or: source "$HOME/.cargo/env"
```

| Task | Command |
|------|---------|
| Everything CI runs | `make check` (fmt check + clippy `-D warnings` + tests) |
| Format | `make fmt` (writes) / `cargo fmt --all --check` (verify) |
| Lint | `cargo clippy --all-targets -- -D warnings` |
| Test | `cargo test --all` |
| Run | `cargo run -p gsp -- --config config.example.yaml` |
| Validate a config | `cargo run -p gsp -- --config <file> --check` |
| Reload a running proxy | edit the config file, or `kill -HUP <pid>` |

`GSP_LOG` sets the log filter (`GSP_LOG=debug`, `GSP_LOG=gsp_core::health=trace`, …).

---

## Guardrails

### Hard rules — do not break these

1. **Run `make check` before every commit.** CI fails on `cargo fmt` diffs and on any
   clippy warning (`-D warnings`). No exceptions, no `#[allow]` to silence a real lint
   without a one-line justification comment.
2. **No `unsafe`.** The codebase currently has zero. `socket2`/`nix` give safe
   wrappers for the syscalls we need. If `unsafe` ever becomes genuinely necessary, it
   needs a `// SAFETY:` comment *and* a note in `HANDOVER.md`.
3. **The `Snapshot` is immutable after `Snapshot::build`.** Never mutate it in place.
   A config/backend change = build a new `Snapshot` and `ArcSwap::store` it. Only
   `reload.rs` writes the swap; everything else reads.
4. **Nothing on the hot path may block or lock.** Hot path = per-connection accept,
   per-connection pump, and (phase 2) per-datagram handling. No blocking syscalls, no
   `std::sync::Mutex` held across `.await`, no per-iteration heap allocation in the
   copy loop. `Backend::observe` takes a short `Mutex` — that is deliberate and only
   runs on connect success/failure, not per byte. Keep it that way.
5. **Control plane may block; keep it off the data plane's threads conceptually.**
   Health checks, reload, discovery, admin API — these can do slow things. They must
   never stall accept/pump.
6. **All metric names go in `crates/gsp-core/src/metrics_defs.rs`** as `pub const`,
   and get documented in [`docs/06-operations-observability.md`](docs/06-operations-observability.md).
   Never inline a metric-name string literal at a call site.
7. **Crate boundaries:** `gsp-config` depends only on `serde` + `thiserror`.
   `gsp-core` has no HTTP / CLI / `axum` dependency — that belongs to `gsp`.
8. **Commit only when the user asks.** If not on a feature branch, branch off `main`
   first. End commit messages with the `Co-Authored-By:` and `Claude-Session:`
   trailers. **Do not `git push`** — credentials are not available in this environment
   unless the user has set them up.

### Soft rules — the intended way to work

- **Latency first.** Every design choice is weighed against added RTT (see NFR N1/N2
  in [`docs/01-requirements.md`](docs/01-requirements.md)). If you spawn a task or add
  a hop per connection, say so in the PR/commit and in `HANDOVER.md`.
- **Agnostic core.** Game-specific knowledge only ever lives in optional sniffer
  plugins or in an external resolver — never in `gsp-core`'s routing/forwarding paths.
- **Incremental, vertical slices.** Follow the phase plan in
  [`docs/08-roadmap.md`](docs/08-roadmap.md). Don't half-land a phase (e.g. don't add
  a UDP listener type without the session table). Update the roadmap's status legend
  when a phase item lands.
- **Tests with every behavioural change.** Unit tests in the module; end-to-end
  forwarding behaviour in `crates/gsp-core/tests/`. Integration tests may use
  `127.0.0.1:0` ephemeral sockets; no other network access.
- **Pre-1.0: prefer clean breaks over compatibility shims.** When you change the
  config schema, update *all four*: `gsp-config` types + `validate()`,
  `config.example.yaml`, and `docs/05-configuration.md`.
- Keep comments at the density of the surrounding code. Module-level `//!` docs should
  say what the module is for and note deliberate simplifications.

### When you touch X, also touch Y

| Change | Also update |
|--------|-------------|
| Config schema | `gsp-config` raw+resolved types, `validate()`, `config.example.yaml`, `docs/05` |
| New metric | `metrics_defs.rs`, `docs/06` |
| New routing matcher / balancer | `docs/03`, `config.example.yaml`, tests |
| New / changed sniffer seam | `gsp_core::sniff`, `docs/03`, `docs/08` (Phase 9). NB: no game sniffers are compiled in — they load as plugins (Phase 9), never as core code or a fork. |
| Finished a roadmap item | status legend in `docs/08-roadmap.md`, `README.md` status block, `HANDOVER.md` |
| New per-connection task or hop | `HANDOVER.md` "latency ledger" note |
| Architectural decision | ADR table in `docs/09-technology-choices.md` |

---

## Architecture invariants

- **Data plane / control plane split.** Data plane reads an immutable `Snapshot` via
  `ArcSwap::load`; control plane builds new snapshots. No shared mutable config.
- **One accept task per (listener × worker)**, each with its own `SO_REUSEPORT`
  socket. Sessions are pinned to their accepting worker (matters for phase 2's
  thread-local UDP session table).
- **`BackendGuard` = one reserved session slot.** Held for the whole connection,
  released on `Drop`. It also carries passive health signals (`guard.observe(ok)`).
- **Health is advisory and eventually-consistent.** `healthy` is an `AtomicBool`;
  reads are lock-free. `rise`/`fall` thresholds live on the `Backend`.
- **Reload carries backend health across the swap by address.** New backends start
  optimistically healthy; the checker corrects within one interval.
- **No cross-instance shared state** (v1). Proxy instances are independent; HA is an
  anycast/L4-LB concern in front.

---

## Roadmap position

Phases 0–2 are done: TCP + UDP forwarding, round-robin + least-conn, active
`tcp_connect` / `udp_probe` health checks, per-backend caps, worker-local UDP
session tables with `src_ip` affinity, hot reload, metrics. **Phase 3 (routing
intelligence) is in progress**: slices 1–8 landed a priority-ordered `routes:`
list with `always` / `client_cidr` / `dst` / `port` / `first_bytes` (`prefix` +
`length`) / `sni` matchers, the `consistent_hash` balancer (rendezvous hash,
pool `hash_on`), the UDP `prefix:` listener (one wildcard `IP_PKTINFO` socket
per routed prefix, via `nix` — still zero `unsafe`) + TCP `freebind:`, and the
sniffer API **seam** (`gsp_core::sniff` — trait + `sniffer` matcher, **no
built-in sniffers**; the loader is roadmap Phase 9). Next is `first_bytes` regex
and the `/route-hint` push resolver, which close phase 3. See `HANDOVER.md` and
`docs/03`. Don't half-land a slice.
