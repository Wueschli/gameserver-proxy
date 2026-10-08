# HANDOVER

Current state and the traps. This file is the *current-state + gotchas* layer only.
Design is the source of truth in [`docs/`](docs/); locked decisions are the ADR table
in [`docs/09-technology-choices.md`](docs/09-technology-choices.md). Per-slice
implementation history lives in `git log` and [`docs/08-roadmap.md`](docs/08-roadmap.md),
not here.

Last updated: 2026-10-07.

## Current state

**All roadmap phases (0–14) are built, individually verified live, and covered by
`make check`.** Remaining work is the [open GitHub issues](https://github.com/wayhouse-proxy/wayhouse/issues). Every CI job
blocks except the two informational security scans, `trivy` and `audit` (2026-10-02,
owner's call).

### Resume here

**Releases:** `v0.1.0-rc.1` and `v0.1.0` are published to GHCR (six private images, amd64 + arm64;
`release.yml` ran end to end, #177 checked: the per-arch digests equal the multi-arch index's).
Tags stay manual (release-please runs with `skip-github-release`); the steps are in
[`RELEASING.md`](RELEASING.md). Automating the tag is backlog #210.

**Sniffer registry (Wave 3, #183):** the `wayhouse-registry` crate (index, verify, `wayhouse-registry-gen`),
the UI backend (`/api/registries/*`, `--registries-file`, `--no-default-registry`) and the Sniffers page
("Install from a registry") are built; see [`docs/sniffers.md`](docs/sniffers.md). Not yet: the sniffers
repo bootstrap and move, the official minisign key (`OFFICIAL_PUBKEY` is `None`, so installs are unsigned),
`min_proxy` enforcement (the aggregator reports no proxy versions).

**Sniffer updates and rollback (Wave 3, #184):** an upload keeps the replaced module as `.<name>.wasm.prev`,
`POST /admin/sniffers/{name}/rollback` (fleet: `/fleet/sniffers/{name}/rollback`) swaps it back, the loader
falls back to it when the current file fails validation, and the Sniffers page has an on-demand "Check for
updates", "Update to X", "roll back" and a `fallback active` badge; see [`docs/sniffers.md`](docs/sniffers.md)
"Updates and rollback".

**Plugin host (Wave 5, slice 1):** `wayhouse-plugin-abi` and `wayhouse-plugin-host` load a plugin, enforce its embedded
capabilities and limits, and run `init` and `on_timer` with the `log` and `state` imports; see [`docs/plugins.md`](docs/plugins.md)
and the [plan](docs/superpowers/plans/2026-10-08-plugin-host-foundation.md). Slice 2 added compile bounds, the bounded `CompilePool` (#265) and the `wayhouse-plugin-check` conformance binary (#266); see the [plan](docs/superpowers/plans/2026-10-08-plugin-compile-bounds-and-check.md). Slice 3: a standalone controller started with `--plugins` stores plugin installs and module blobs and serves `/plugins` (upload, approve, list, enable, delete); see the [plan](docs/superpowers/plans/2026-10-08-plugin-install-store-and-api.md). Slice 4: the standalone controller ticks enabled plugins (`on_timer`), keeps their state with a revision-checked commit and serves `/plugins/{id}/status`; the controller now compiles through the pool (closes #265); see the [plan](docs/superpowers/plans/2026-10-08-plugin-tick-runner.md). Secret storage is reviewed in [`docs/superpowers/specs/2026-10-08-plugin-secret-storage-design.md`](docs/superpowers/specs/2026-10-08-plugin-secret-storage-design.md) (#235, design only; no secret-using plugin ships before it is built). Not yet: the HA leader tick with a term-checked commit, the HA-replicated install state,
`http` (needs the secret-storage implementation, designed in #235), `routes` (#236), webhooks and events (#237), `backends`, the UI, registry `kind = plugin`.

The repository is **public** since 2026-10-03 (history scanned, clean), so
GitHub-hosted Actions minutes are free. The nightly CI run (`schedule` in `ci.yml`,
03:17 UTC) is on again; it exercises the nightly-only paths (the in-Docker
`BIN_SOURCE=builder` deploy build) and runs `fuzz`/`deploy` as canaries for commits
that don't touch their paths.

Owner decisions that still hold:

| Question | Decision |
|----------|----------|
| Native TLS on the fleet HTTP servers | Built (2026-10-02): controller, aggregator and UI via `--tls-cert`/`--tls-key`, `wayhouse`'s admin API via `settings.admin.tls`. New client code must not hard-code `http://` and must build clients via `wayhouse_http::{client, builder}`. |
| Publish the reference images | Docker only, multi-arch (amd64 + arm64, native runners; arm64 also built nightly), to GHCR on `vX.Y.Z` tags (`.github/workflows/release.yml`, 2026-10-04; gated on tag = workspace version, green CI on main, and a built + smoked image set before any push, #112). Kubernetes packaging: [#88](https://github.com/wayhouse-proxy/wayhouse/issues/88). |
| Self-hosted CI runner | Not while the repo is public (GitHub advises against self-hosted runners on public repositories, since fork PRs can run code on them); the switch (PR #21) was closed. |

**CI timings, measured** (2026-10-02). Cold (lockfile changed): `test` 8m15,
`build-release` 16.5 min, each `tunnel` leg ~10 min, ~22 min wall clock. Warm: `test`
4m41, `build-release` 6m10, `tunnel` 6m47 (kernel) / 8m12 (userspace), `sniffers` 3m12,
~9.5 min wall clock. If a code push without a lockfile change is back near the cold
numbers, the cache key in `.github/actions/cargo-cache` is the first suspect.

Never verified outside CI:

- `make deploy-images` / `make deploy-smoke` and the compose **tunnel** override were
  only ever exercised by the CI `deploy` job. On a machine with Docker, run both once.
- The k8s manifests were only schema-validated (kubeconform), never applied to a cluster.

Watch-list:

- **Runner image:** every CI job is pinned to `ubuntu-24.04`, so the 2026-10-19
  `ubuntu-latest` → Ubuntu 26 switch changes nothing. Moving to 26 later is a deliberate
  edit of all jobs together: re-check `tunnel` (AppArmor userns sysctl, `wireguard`
  module) and the release jobs' glibc vs. the distroless runtime (2.41).
- **Trivy pinning:** CI runs the official `aquasec/trivy` image pinned by digest, not
  `trivy-action`/`setup-trivy`, whose tags were hijacked in March 2026. Re-check the
  binary against the release checksum when bumping the digest. The `trivy` job also
  `cosign verify`s the digest's keyless Sigstore signature (identity: Aqua's release
  workflow at a version tag) on every run, so a bump to an unsigned digest fails there.
- **cargo-auditable:** release binaries (CI `build-release` and the Dockerfile builder)
  are built with `cargo auditable build`, version 0.7.7 in both; bump together.
- **Merge queue:** PRs land one at a time, each re-run on the latest `main`. If several
  PRs are routinely in flight at once, GitHub's merge queue (needs an
  `on: merge_group` trigger) would keep `main` green.

Whether a red `tunnel`/`deploy` also *blocks merging* depends on GitHub
branch-protection required checks, a repo setting outside this tree.

### Recent landings

Newest first. Each feature's design lives in the linked chapter, ADR or spec; the
details are in `git log`.

| Date | Feature | Where it is documented |
|------|---------|------------------------|
| 2026-10-04 | Live tunnel address change (agent and `wayhouse` re-address the interface without a restart) | `docs/11` "Address authority" ("Address changes") |
| 2026-10-03 | CI change detection from `cargo metadata` (`.github/scripts/changes.py`) | [spec](docs/superpowers/specs/2026-10-03-ci-change-detection-design.md), AGENTS.md "CI" |
| 2026-10-03 | Edge restarts keep the tunnel up (`boot_id` on registrations) | `docs/11` "Edge restarts" |
| 2026-10-03 | `wayhouse --aggregator-admin-url` (fan-out through NAT, port maps, TLS terminators) | `docs/12` |
| 2026-10-03 | Tunnel addresses page in `wayhouse-ui` (read-only; Release button added 2026-10-04) | `docs/10` "The admin GUI" |
| 2026-10-03 | TLS handshake flood limits (`wayhouse_http::tls::HandshakeLimits`) | ADR 29, [spec](docs/superpowers/specs/2026-10-03-tls-handshake-limits-design.md) |
| 2026-10-03 | HA write forwarding bounded by a 10 s timeout (`504` on a hung leader) | `docs/10` |
| 2026-10-02 | `cargo audit` and Trivy scans in CI (informational) | ADR 28, AGENTS.md commands table |
| 2026-10-02 | Native TLS for every fleet HTTP server | ADR 27, `docs/12` "Native TLS", [spec](docs/superpowers/specs/2026-10-02-native-tls-other-servers-design.md) |
| 2026-10-02 | TLS-capable HA peers (`--ha-peers id=https://…`) | `docs/12` "HA replicas over TLS", [spec](docs/superpowers/specs/2026-10-02-ha-tls-peers-design.md) |
| 2026-10-02 | `--ca-file` on every binary; new crate `wayhouse-http` | ADR 26, `docs/12`, [spec](docs/superpowers/specs/2026-10-02-custom-ca-design.md) |
| 2026-10-02 | Tunnel address authority (`wayhouse-controller --tunnel-network`) | `docs/11` "Address authority", [spec](docs/superpowers/specs/2026-10-02-tunnel-address-authority-design.md) |
| 2026-10-01 | `deploy/`: reference images, Compose demo, k8s manifests | `docs/12`, [`deploy/README.md`](deploy/README.md) |

## What's built

### Data plane — `wayhouse` + `wayhouse-core` + `wayhouse-config` (phases 0–9 + two follow-on passes)

- TCP + UDP transparent forwarding, backend pools, active health checks, hot config
  reload (file watch / `SIGHUP` / admin-triggered).
- Routing matchers: CIDR / port / first-bytes / SNI / sniffer. Balancers: `rr` /
  `least_conn` / `weighted` / `consistent_hash`. ClientHello reassembly across
  segments; `RouteHint.reject` hard drop.
- Filters: CIDR ACL, MaxMind GeoIP, per-listener token-bucket rate limit,
  per-source concurrent conn/session cap, process-wide caps.
- PROXY protocol v1/v2 (incl. `v2-udp`); transparent mode (TPROXY /
  `IP_TRANSPARENT`, v4 + v6); listener port-range bind (`bind: "host:lo-hi"`,
  ≤1024 ports, one socket/port × workers).
- External resolvers (HTTP / gRPC, TTL-LRU cache) and backend discovery sources
  (DNS SRV / Consul / Kubernetes / tunnel) behind live-reloadable `ArcSwap` seams.
- Sniffers via `wasmtime` — no game-protocol code in core. First-party
  `a2s` / `minecraft` / `regex_firstbytes` etc. sniffers in `wayhouse-proxy/sniffers` (this repo
  keeps the ABI crate `crates/wayhouse-sniffer-abi/` and pins their releases in `sniffers.lock`).
  Per-sniffer config, benchmarked p50 ~8–10 µs.
- Perf pass: `splice(2)` zero-copy TCP pump, `recvmmsg(2)` UDP ingress batching,
  single-level timing-wheel UDP idle expiry.
- Ops: `wayhouse_build_info{component,version,commit}`, `wayhouse_fd_open` / `wayhouse_fd_limit` sampling.

### Distributed control plane — phases 10–13 (fully built)

- **`wayhouse-controller`** — Tier-1 config + operator-intent distribution. `sled`-backed
  revision logs, `GET /config/subscribe` (SSE) catch-up-then-tail,
  revisions / diff / rollback, optional `--auth-token` bearer gate. Fleet hierarchy
  (`standalone` / `slave` relay `--role`), post-install adoption
  (`POST /admin/adopt`), intra-tier HA (embedded `openraft`, real 3-node cluster,
  transparent leader HTTP-forwarding — clients stay HA-unaware), staged/canary
  rollout (`?stage=canary&group=`, `POST /config/promote/{rev}`), per-revision
  `actor` audit field.
- **`wayhouse-aggregator`** — leaf→root fleet-state push (`POST /ingest` →
  `GET /fleet/pools|sessions|healthz|subscribe`), aggregator-of-aggregators
  hierarchy (namespaced `tier/instance`), intent-verb fan-out (targeted
  drain/undrain + broadcast backend/route-hint edits), `--auth-token` (`/fleet/*`) /
  `--ingest-token` (`POST /ingest` only) / `--instance-token` (outbound) gates; a pushed
  `admin_url` must be the pusher's own IP or match `--instance-url-allow`. No `wayhouse-core`/`wayhouse-config` dep; carries no durable state.
- **`wayhouse-ui`** — dedicated BFF process (not hosted in controller/aggregator).
  Session-cookie auth, proxies both upstream APIs forwarding response headers,
  `GET /ws/fleet` live feed off one shared aggregator SSE subscription, RBAC
  (`viewer` < `operator` < `admin` via `--users-file` argon2 hashes; legacy
  single `--ui-password` kept). React + Vite + TS frontend in `crates/wayhouse-ui/web/`
  (`make ui`, `--static-dir`) — redesigned in `d86c786` (Tailwind + Radix,
  react-router, grouped fleet tree via `settings.group`, schema-driven settings
  form over a raw-YAML escape hatch, Sniffers page backed by `GET/POST/DELETE
  /admin/sniffers`). Destructive actions (drain, remove backend, set a backend
  `draining`/`disabled`, rollback, sniffer remove, applying Settings) confirm first via
  `useConfirm()`; action buttons disable while a request is in flight; a
  vitest suite (`make ui-test`, CI `ui` job) pins that, a Playwright smoke test
  (`make ui-e2e`, same job, backend stubbed with `page.route`) drives the Settings
  confirm in a real browser, and
  `wayhouse-ui`'s `header_contract` test pins that every proxied route forwards
  status + response headers (**add new proxied routes to its `ROUTES` table**).
- **Tier-2 regional health gossip** — embedded `foca` SWIM mesh over one HMAC-auth
  UDP socket, per-backend last-writer-wins health broadcast piggybacked on foca's
  own anti-entropy, quorum-based `Backend::domain_down` override (additive to the
  local `healthy` flag; only a local `rise` streak clears `healthy`). Spawned only
  when `settings.gossip` is set.
- **`crates/wayhouse-fleet-tests`** — spawns the real `wayhouse` / `wayhouse-controller` /
  `wayhouse-aggregator` binaries as child processes over real HTTP; part of `make check`
  (adds ~15–20 s).

### Backend transport — phase 14 (`docs/11`, ADR 25)

WireGuard tunnels so a globally-distributed proxy fleet can reach game servers that
are **not** on a shared trusted network. Unmodified WireGuard is the entire tunnel
data plane (kernel module primary, `boringtun` userspace fallback). All 7 slices
built; verified live in 4 Docker containers (`--cap-add=NET_ADMIN
--device=/dev/net/tun`).

- **`wayhouse-agent`** — origin-side binary (own workspace crate). Brings up one local
  WireGuard interface via `defguard/wireguard-rs`, persists a stable keypair
  (`<data_dir>/private.key`, `0600`, never rotated by this code), registers its
  pubkey + fronted backend addresses with the controller's backend-peers registry
  on a fixed interval, and subscribes to the proxy-peers registry to reconcile every
  proxy onto its interface.
- **`wayhouse-controller`** — two registries alongside the config/intent logs, each its
  own `sled` db: `peers` (backend-peers — origins register, proxies subscribe) and
  `proxy_peers` (the mirror — proxies register, agents subscribe). `POST` / `GET` /
  `GET …/subscribe` on each; validated (pubkey base64-decodes to 32 bytes, backends
  parse as `SocketAddr`) before the store.
- **`wayhouse` `--tunnel-*`** — brings up the shared WireGuard interface *before* any
  listener/source starts (an ordering bug fix — a failing `--tunnel-*` must leave
  zero listeners bound), subscribes to backend-peers and reconciles every origin
  onto the interface's peer list (add/update only, never remove — no "gone for
  good" signal yet), and registers itself into proxy-peers.
- **`tunnel` `BackendSource`** — `GET {controller}/peers/{origin}` through the same
  discovery reconcile path as `dns_srv`/`consul`/`kubernetes`. Every fetch
  re-verifies the registry's current pubkey matches `backend_sources[].pubkey`
  (a key change is refused loudly); `404` is `Ok(empty)` = keep last-known-good.
- **Data plane needs zero changes** — once an origin's interface exists on the proxy
  host, its backends are ordinary routable addresses; `connect_backend` /
  `connect_upstream` / health dials don't know a tunnel is involved.

## Known flakes & environment gotchas

- **Disk fills up in long sessions.** `target/debug` grew to ~30 GB over many test
  builds on 2026-10-02 (the linker then fails with exit 1, not a clear "no space");
  `target/debug/incremental` alone was 12 GB. `rm -rf target/debug/incremental` is the
  cheap fix, `rm -rf target/debug` the full one (one cold rebuild).
- **`make deploy-lint` and the locale.** Its ruby checks read the Dockerfile as
  US-ASCII under a `C`/POSIX locale and fail with `invalid byte sequence`; run it with
  `LANG=C.UTF-8 LC_ALL=C.UTF-8` (CI's runners are UTF-8 already).
- **`make audit`** runs `cargo audit` over both lockfiles (root and
  the fuzz harness; `.github/scripts/cargo_audit.sh`, JSON in `target/cargo-audit/`;
  needs `cargo install cargo-audit --locked`). In CI since 2026-10-02 as the
  **informational** `audit` job (every push/PR + nightly; summary table, warnings,
  `cargo-audit` artifact).
  New advisories land on a schedule you don't control — on 2026-10-01 it caught
  `rustls` (RUSTSEC-2026-0285) and two `wasmtime` fuel-accounting advisories
  (RUSTSEC-2026-0315/0316), fixed by lockfile-only patch bumps (`rustls` 0.23.45,
  `wasmtime` 48.0.3). Three "unmaintained crate" warnings (`atomic-polyfill`,
  `fxhash`, `instant`) are informational and don't fail the target; they show in
  the `audit` job's summary (still present on 2026-10-02 — replacing them means moving off
  the crates that pull them in).
- **Fixed-sleep test flakes (fixed 2026-10-01)**: `resolver_target_gets_a_proxy_protocol_header`
  and `acl_deny_drops_the_connection_before_routing` slept 150 ms then `connect().unwrap()`,
  which loses to listener startup under parallel load; they now retry the connect
  until the listener is up. `wayhouse-controller`'s `seeded_entries_are_readable_and_persist_across_a_reopen`
  reopened `sled` immediately after dropping it (the file lock is released on a
  background thread); it now retries the open. **The other ~30 `sleep(150ms)` +
  connect tests in `crates/wayhouse-core/tests/tcp_forward.rs` have the same shape** —
  if one flakes, reuse `connect_when_listening` there rather than lengthening the sleep.
- **sled lock flake (fixed 2026-10-04)**: `ha::import::tests::set_aside_moves_only_unmarked_non_empty_dirs`
  failed now and then with `WouldBlock` ("could not acquire lock"). Same cause as above, and
  it was a **production** race too, not only a test one: `set_aside_pre_ha` opens and drops
  a registry store to inspect it, renames the directory, then reopens it to count it, and
  `read_pre_ha` opens it again once the cluster initializes, all in one process, while
  sled's background threads can still hold the dropped store's lock. The import's opens
  now go through `store::retry_when_unlocked` (bounded 5 s, lock error only, also matched
  under `anyhow` context); `set_aside_waits_for_a_lock_that_is_about_to_be_released` holds
  the lock for 300 ms and failed before the fix. The flake itself was not reproduced
  locally (100 runs under load), so the link to the CI failure follows from the cause.
- **UDP idle-eviction test flake (fixed 2026-10-04)**: `an_active_session_survives_past_its_idle_window_then_expires`
  (`wayhouse-core/tests/udp_forward.rs`). Two causes. The echo backend is UDP-only, so the pool's default
  `tcp_connect` health check failed against it and (`fall: 3` every 2 s) marked it unhealthy about 4 s in;
  the test waits for an eviction right around then and got "no healthy backend" instead of the freed
  slot. And the first datagram raced listener startup after a fixed 150 ms sleep. The two idle tests
  now use a `udp_probe` check, and the first datagram is retried until it is echoed. Any new UDP
  test against `echo_backend` needs a `udp_probe` health check if it runs past ~3 s.
- **Lints and CI pins (2026-10-04)**: the root `Cargo.toml` has a curated
  `[workspace.lints.clippy]` (`redundant_closure_for_method_calls`, `needless_pass_by_value`,
  `items_after_statements`, `manual_let_else`, `default_trait_access`); every workspace
  crate opts in. `cast_possible_truncation` is left out: ~30 of its hits are test code, and
  the production ones are the vetted `u128` millis casts. Every third-party action in
  `.github/` is pinned to a commit SHA with the tag in a trailing comment (the `stable` and
  `nightly` toolchain branches of `dtolnay/rust-toolchain` too); bump them by hand.
- **A config file directly under `/tmp`** triggers continuous ~200 ms
  `configuration reloaded source=file` log spam (a `notify` / tmpfs
  mtime-granularity interaction). Harmless — `reload.rs` only swaps the `Snapshot` —
  but run live tests from a real directory; a real deployment's config isn't on tmpfs.
- **Rust handler ↔ TS frontend wire-shape mismatches** are a recurring bug class
  (hit ≥3×: a pass-through proxy silently dropping every upstream response header;
  `JSON.parse("ok")` on a text body). A thin proxy needs an explicit test that a
  header survives the hop — status + body assertions don't catch it.
- **`shutdown_grace_sec: 30`** in `config.example.yaml` means `timeout N wayhouse` does
  not kill at `N` (SIGTERM starts a graceful drain that can run the full 30 s). Use
  `timeout -s KILL` and give helper processes a longer lifetime than the proxy in
  smoke scripts. Bare `kill`/`pkill` on background test processes has also produced
  a stray "Exit code 144" from the Bash tool in this environment — wrapping each
  process in `timeout -s KILL` instead avoids it. **Never `pkill -f <pattern>` from an
  agent shell:** the pattern also matches the shell's own command line and kills it; use
  `pkill -KILL -x <exact-process-name>` (`wayhouse`, `wayhouse-controller`, `wayhouse-agent`, …).
- **`--tunnel-*` / `wayhouse-agent` need `CAP_NET_ADMIN` + `/dev/net/tun`** (even
  `boringtun` userspace does, for the TUN device). In containers grant both
  (`docs/12`). The rootless tunnel e2e lab gets `CAP_NET_ADMIN` from a user namespace
  (`unshare -Urnm`), so it runs without root or Docker — including in the original dev
  sandbox. `TunnelSource` origin-name matching: `backend_sources[].name` must equal the
  origin's `wayhouse-agent --name` (intentionally the same string).
- **Phase 14 tunnel e2e** — `make tunnel-e2e` (`crates/wayhouse-fleet-tests/tests/tunnel.rs`,
  CI job `tunnel`, kernel + userspace matrix) runs the real `wayhouse-controller` /
  `wayhouse-agent` / `wayhouse --tunnel-*` binaries in rootless network namespaces
  (`unshare -Urnm`; no Docker, no root). It replaced the one-off Docker harness.
  The CI job is **blocking** since 2026-10-02. It is green on both backends on GitHub (`unshare -Urnm`
  + tmpfs on `/run` and the `wireguard` module work on the Ubuntu runners). The lab now has
  **12 scenarios** (namespace helpers, TCP/UDP round trip, stays-up-across-re-registrations,
  a proxy added later, **two proxies sharing one origin**, a pinned-key mismatch, a pinned
  address collision, an edge restart keeping its address, an edge restarting with the
  controller down) and takes ~4–5 min per backend locally, ~9.5 min per leg in CI (measured
  before the 2026-10-03 boot id fix removed the kernel edge-restart scenario's 200 s
  wait; expect that leg to be ~2.5 min shorter now). Run it with
  `TUNNEL_BACKEND=kernel|userspace make tunnel-e2e` (plain `cargo test`) or
  `make tunnel-e2e-ci` (nextest + JUnit; needs `cargo install cargo-nextest --locked`). Needs `unshare`, `ip`,
  `nsenter`; the userspace backend also needs `/run/wireguard`
  (the make target mounts a tmpfs on `/run` for it). Traps it taught:
  **`/pools` health is optimistic** (a new backend is `healthy` before the tunnel is
  up — wait for a real round trip); **userspace (`boringtun`) first handshake used to take
  ~25 s** (the proxy has no endpoint for the origin, and `boringtun` arms a peer's
  persistent keepalive only 25 s after the peer is created, while the kernel sends one
  at once; kernel is ~2 s). The agent now sends one empty UDP datagram through the
  tunnel right after configuring a proxy peer (`interface::kick_handshake`), which
  starts the handshake immediately — fix written 2026-10-04, confirmed by
  the `tunnel (userspace)` CI leg (scenario 1 passes in ~8 s, down from ≥25 s for the
  handshake alone; the lab's userspace deadline stays 90 s because `an_edge_restarts_with_the_controller_down` takes ~37 s there); tracked in
  [#67](https://github.com/wayhouse-proxy/wayhouse/issues/67);
  dead namespaces' veths disappear asynchronously, so test namespaces never reuse
  names within a run. Slice 7 (proxy-peers registry) is live-verified, including two
  proxies carrying traffic at once.

## Open follow-ups

Everything not built or not yet fixed is tracked as a
[GitHub issue](https://github.com/wayhouse-proxy/wayhouse/issues), not in this file.
Check the issue list before starting new work, and file new follow-ups there rather
than here. Where the items that used to live here went:

- v2 design ideas (CGNAT, gossip load signals, `failure_domain` discovery; HA with
  `--role slave` is built): [#65](https://github.com/wayhouse-proxy/wayhouse/issues/65)
- Open design questions: [#69](https://github.com/wayhouse-proxy/wayhouse/issues/69)

## Workflow gotcha: run `cargo fmt --all` as its own step before `make check`

`make check` only reports formatting diffs, so an earlier fmt pass does not cover later edits.
See [`CONTRIBUTING.md`](CONTRIBUTING.md#build-and-test). Piping `make check` through `tail` hides
an early `fmt-check` failure; read from the top or check the exit code.

---

## Invariants that bite if you forget them

(Full list in [`AGENTS.md`](AGENTS.md) "Architecture invariants". These are the
ones with a subtlety.)

- **Exactly one place builds + stores a `Snapshot`: `reload::apply`.** Runtime
  edits (backend overlay, discovery refresh, admin) are just extra *rebuild
  triggers* — they mutate a persistence layer (`BackendOverlay`, `Discovery`)
  then call `request_reload()`. Backend *state* (`PATCH`) mutates atomics on the
  live `Backend` and needs no rebuild.
- **`Sniffers` and `Resolvers` registries are `ArcSwap`-backed** (unlike the
  snapshot, which every worker re-`load`s itself). Every worker clone points at
  the *same* instance, so `replace()` from the reload task is visible instantly
  with no replumbing.
- **Listeners are reconciled by name** (`ListenerManager::reconcile`), not
  rebuilt with the snapshot. An unchanged listener keeps running with its route
  rules captured at spawn; only the *pool contents* they resolve to are read
  live. A `ListenerConfig` change (bind, protocol, routes, `prefix`,
  `freebind`, `route_hint`, ACL, `rate_limit`, `geo`, `transparent`) stops and
  re-spawns it — a same-bind rebind is gapless via `SO_REUSEPORT`.
- **Startup-only config** (reload does *not* re-read): `workers`,
  `settings.limits`, `settings.geo_db`, `settings.sniffers` engine params
  (`call_timeout_ms` / `max_memory_bytes` — only the `dir` *contents* are live).
- **Zero `unsafe`.** `socket2` 0.6 (`SockRef` for `IP_TRANSPARENT` v4/v6,
  `IP_FREEBIND`), `nix` (`IP_PKTINFO` / `IP_ORIGDSTADDR` cmsgs, `recvmmsg`,
  `splice`) — all safe wrappers.
- **UDP amplification guard**: the proxy never sends to a client without an
  established session; a dropped datagram (routing / ACL / rate / gate) gets no
  reply. Covered by `crates/wayhouse-core/tests/amplification.rs`.

---

## Latency ledger

Per-connection / per-datagram cost of every feature. **If you add a
per-connection or per-datagram task, hop, or allocation, add it here.**

### Steady state (per byte / per datagram)

- **TCP pump**: on Linux, `splice(2)` `socket → pipe → socket` — no userspace
  copy, no 32 KiB×2 heap buffers (the pipe fds cost ~2 KiB kernel each). Up to 2
  extra syscalls per direction per 64 KiB chunk. Non-Linux / `pipe2` failure:
  buffered `try_read`/`try_write`, one 32 KiB buffer per direction. No lock.
- **UDP ingress**: a share of one `recvmmsg` (≤16 datagrams/syscall on Linux;
  fresh `MultiHeaders` amortised over the batch), one `HashMap` lookup by client
  `SocketAddr`, one relaxed atomic store (liveness), one `send` upstream. No
  lock, no task spawn. Consecutive datagrams of one session go upstream in one
  `sendmmsg` (one small `Vec` of slices per run). The reply path is one
  `recvmmsg` + one `sendmmsg` per wakeup (ADR 31).
- Recv buffers: `RECV_BATCH` (16) × 64 KB **per worker** (shared across that
  worker's sessions) + 16 × 64 KB per worker thread for the reply pumps
  (`ReplyBatch`, thread-local; not per session).
- Established UDP sessions skip the ACL / rate-limit / geo / gate checks
  entirely — those run only for datagrams that miss the session table.

### Per new TCP connection / new UDP session (paid once)

- **Base**: 1 `Pool::acquire[_for]` (lock-free reads + one atomic add), 1
  upstream `connect`, 1 spawned pump/reply task, (UDP) 1 socket `bind`+`connect`
  + session-table insert.
- **Routing**: 1 `local_addr()` syscall + a linear scan of the small route list
  (bit-compare per `client_cidr`/`dst`, `u16` range per `port`, `starts_with` +
  len check per `first_bytes`, one `extract_sni` pass per `sni`).
- **UDP external resolver** (first route match is a `resolver`): the call runs in
  a spawned task (one per pending session, `JoinSet` on the worker), never on
  the recv loop. Pending sessions are capped at 1024 and 1 MiB buffered per worker, and hold up to
  4 datagrams each (one `to_vec` per buffered datagram); overflow drops as
  `pending_full`. Static-pool routes and route hints stay inline, no spawn.
- **Peek** (only if a route uses `first_bytes`/`sni`/`sniffer`): one `MSG_PEEK`
  into a `peek_len()`-sized `Vec` (`PEEK_MAX` = 4096 for `sni`/`sniffer`), 250 ms
  budget, + an `Arc<ListenerConfig>` clone into the per-conn task. Re-peeks every
  5 ms only if the first byte is a TLS record and the first record isn't whole.
- **`consistent_hash`**: `O(healthy)` — one `sort_by_key` with a `DefaultHasher`
  per backend over the healthy `Vec` every balancer already builds. Same work as
  `least_conn`. Process-stable hash only (fine: reload rebuilds pools, no
  cross-instance state).
- **`route_hint: true`**: one `ArcSwap::load` + `HashMap::get` (lock-free) + one
  `String` clone when a hint is present. Listeners without the flag pay nothing.
- **External resolver**: one `.await`ed HTTP/gRPC round-trip (`timeout_ms`,
  default 40 ms) on the routing path, in the spawned task — never the accept
  loop. With `cache:` a repeat key is a `Mutex<LruCache>` get instead. A
  `target` connection skips `Pool::acquire_for` entirely — cheaper than pooled.
- **Sniffer** (`WasmSniffer`): one `Store::new` + `Instance::new` (fresh
  per call) + `memory.write` of the peeked bytes + one guest call + decode, all
  synchronous on the per-conn task. Bounded by `call_timeout_ms` (epoch
  interruption) + `max_memory_bytes`. **Benchmarked**: p50 8–10 µs, p99 12–26 µs
  for the three first-party sniffers — comfortably inside NFR N1 (500 µs), so the
  fresh-`Store`-per-call design needs none of the `InstancePre` / pooling /
  warm-instance fallbacks held in reserve. Listeners without a `sniffer:` route
  pay one `HashMap::get`. See `docs/07` "Sniffer sandbox guarantees".
- **Filter chain** — CIDR ACL: a bounded bit-walk of the `deny` (and, if
  non-empty, `allow`) radix trie, ≤32/128 hops, no alloc/lock. GeoIP: one
  MaxMind tree lookup + a small `Vec` scan. Rate limit / `per_source` / global
  caps: one `Mutex<HashMap>` or atomic op, one RAII guard, no `.await`, no alloc.
  Each unconfigured filter is one `is_empty()` / `is_enabled()` bool check.
- **UDP first-packet gate**: reuses the already-computed sniff hint + a short
  route-list scan for a `FirstBytes` match. No lock/alloc/task.
- **PROXY protocol** (`v1`/`v2`): one `Vec` (≤52 B) + one extra `write_all` to
  the backend before the pump. `v2-udp`: one `Cow::Owned` on the *first*
  datagram only. `none` pays nothing.
- **Transparent mode**: upstream socket gains one `setsockopt(IP_TRANSPARENT)` +
  one `bind` before connect; UDP also binds one per-session `IP_TRANSPARENT`
  reply socket and uses `recvmsg` + a fixed cmsg buffer. A few extra syscalls at
  setup; nothing per byte.
- **UDP passive health**: one `Arc<Backend>` clone per session; on a `send`/`recv`
  error only, one `kind()` compare and (for `ConnectionRefused`) one
  `Backend::observe(false)`.
- **Connection draining** (`ConnTracker`): one `watch::send_modify` on open and
  on close. Nothing steady-state.

### Control-plane only (zero data-path cost)

Listener reconcile, backend overlay rebuild, backend discovery refresh
(`refresh_loop` per source → `fetch` → `Discovery::store` + `notify` → snapshot
rebuild reads `Discovery::get`).

---

## Codebase map

| File | Responsibility |
|------|----------------|
| `crates/wayhouse-config/src/` (`schema.rs` raw YAML types, `validate.rs` `validate()`, `parse.rs`, `resolved.rs`, `cidr.rs`, `matcher.rs`, `keys.rs`; `lib.rs` re-exports) | Raw YAML types, `validate()`, resolved `Config` / `PoolConfig` / `ListenerConfig` / `ResolverConfig` / `SniffersConfig` / `HealthCheck`; routing (`Matcher`, `Action`, `OnError`, `Cidr`, `CidrSet` trie, `HostPattern`, `MatchContext`, `RouteHint`, `extract_sni`); filters (`Acl`, `GeoAcl`, `RateLimit`, `PerSourceLimit`, `GlobalLimits`). **All schema rules here.** |
| `crates/wayhouse-core/src/snapshot.rs` | `Snapshot { listeners, pools, sources, resolvers, limits, geo_db }`; `build` → `build_with_overlay` → `build_with_sources` carry health/admin-state over by address and apply overlay + discovered backends. |
| `crates/wayhouse-core/src/pool.rs` | `Pool` (balancer, `rr` index, `hash_on`, `weights`; `acquire` / `acquire_for` / `acquire_addr` / `backend`, `hrw_score`), `Backend` (health / active / streaks / `check_kind` + `AdminState`), `BackendGuard` (RAII slot + passive health), `PickError`. |
| `crates/wayhouse-core/src/listener.rs` | `run_tcp_listener`: accept loop; per-conn task does ACL/geo/rate/`per_source`/global-cap checks, first-bytes peek, route match, pool lookup. |
| `crates/wayhouse-core/src/listener_udp.rs` | `run_udp_listener`: per-worker `recvmmsg` batch loop, `(client, Option<SocketAddr> dst)` session table, `IdleWheel` idle expiry, per-session upstream socket + reply pump. `UdpMode` Plain / Prefix (`IP_PKTINFO` + `sendmsg` reply) / Transparent (`IP_ORIGDSTADDR`, client-bound upstream, per-session `IP_TRANSPARENT` reply socket). |
| `crates/wayhouse-core/src/{ratelimit,src_conns,limits,geo}.rs` | Per-listener token bucket / per-source concurrent cap / process-wide caps / MaxMind country lookup. |
| `crates/wayhouse-core/src/sniff.rs` | `Sniffer` trait + `Sniffers` `ArcSwap`-backed registry (`register` / `get` / `replace`) + `warn_if_missing`. **No built-in sniffers** — the seam the phase-9 loader fills. |
| `crates/wayhouse-core/src/route_hint.rs` | `RouteHints` — `ArcSwap<HashMap>` `src_ip → pool` push-resolver table (`POST /route-hint`), lock-free read. |
| `crates/wayhouse-core/src/resolver.rs` | `trait Resolver`, `ResolveRequest` / `Resolution` / `ResolveError`, `Resolvers` (`ArcSwap`-backed) map, `resolve_route` (async route walk), `CachedResolver` (TTL LRU). Transports live in `wayhouse`. |
| `crates/wayhouse-core/src/discovery.rs` + `sources.rs` | `trait BackendSource` + `Discovery` last-known-good cache + `refresh_loop`; `SourceManager` reconciles one refresh task per pool `source` on reload (discovery analogue of `ListenerManager`). |
| `crates/wayhouse-core/src/drain.rs` | `ConnTracker` / `ConnGuard` — `watch<usize>` live count + an `id → {proto, listener, peer, local, pool, backend, since}` registry (`GET /sessions`); `wait_idle()`. |
| `crates/wayhouse-core/src/overlay.rs` | `BackendOverlay` — runtime backend add/remove, layered on file `targets` at rebuild. |
| `crates/wayhouse-core/src/proxy.rs` | `handle_tcp` (pool) / `handle_tcp_target` (resolver `target`, no guard) → `connect_backend` + `pump` (`splice` / buffered). Writes the PROXY protocol header before the pump. |
| `crates/wayhouse-core/src/proxy_protocol.rs` | `header(mode, src, dst)` — PROXY protocol v1 (text) / v2 (binary, STREAM or DGRAM). Write-only. |
| `crates/wayhouse-core/src/health.rs` | 500 ms sweep, probes due backends (`tcp_connect` / `udp_probe`), updates health + `wayhouse_pool_backends` gauges. |
| `crates/wayhouse-core/src/runtime.rs` | `Runtime::start*` builds `ListenerManager` + `SourceManager` + health task; owns `RouteHints` / `ConnTracker` / `BackendOverlay` / `Sniffers` / `reload_requested`. `shutdown_with_grace` = stop listeners + await health + `wait_idle` + `abort_all`. |
| `crates/wayhouse-core/src/listeners.rs` | `ListenerManager` — one task `Group` (workers + `watch<bool>` stop) per listener; `start_all`, `reconcile(&Snapshot)` (diff by name), `stop_all` / `abort_all`. |
| `crates/wayhouse-core/src/net.rs` | `bind_reuseport_tcp` (+ `freebind` / `transparent`), `bind_reuseport_udp` (`UdpMode`), `bind_transparent_udp`, `connect_tcp_from`, `set_ip_transparent` (v4 + v6). |
| `crates/wayhouse-core/src/metrics_defs.rs` | **Every metric name** (`pub const`). |
| `crates/wayhouse/src/main.rs` | CLI (`--config`, `--check`), tracing, runtime bring-up, shutdown. Opens `geo_db` + builds sniffers (fails `--check` / startup on a bad one). |
| `crates/wayhouse/src/admin.rs` | axum router: `GET /healthz` `/readyz` `/metrics` `/pools` `/config` `/sessions`; `POST /route-hint` `/admin/drain` `/admin/undrain`; `PATCH` backend state; `POST` / `DELETE` a backend. `router(state)` split out for the in-module HTTP tests. |
| `crates/wayhouse/src/resolver.rs` | `HttpResolver` (`reqwest`), `GrpcResolver` (`tonic`, `mod pb` from `build.rs`), `build_resolvers`. |
| `crates/wayhouse/src/sniffer_loader.rs` | `SnifferLoader` (shared `wasmtime::Engine` + epoch-ticker thread) + `scan(&SniffersConfig)`; `WasmSniffer`; `build_sniffers` = `new` + one `scan`. |
| `crates/wayhouse/src/discovery.rs` | `DnsSrvSource` (`hickory-resolver`), `ConsulSource` / `KubernetesSource` (`reqwest`), `DiscoveryFactory`. |
| `crates/wayhouse/src/reload.rs` | `SIGHUP` + `notify` file watch + `reload_requested()` → debounce → `apply` (validate, `build_with_overlay`, store, reconcile listeners / sources / resolvers, rescan sniffers). |
| `crates/wayhouse/src/procinfo.rs` | `wayhouse_fd_open` / `wayhouse_fd_limit` — a small detached `/proc/self/fd` sampling task. |
| `crates/wayhouse/proto/resolver.proto` + `build.rs` | gRPC resolver contract + `tonic_build` codegen (needs `protoc`). |
| `crates/wayhouse-controller/src/addresses.rs` + `addresses/api.rs` | Tunnel address authority: `Network`, `AddressBook` (claim / release / entries over two sled trees, one mutex, flush after each transaction), `expand_backends`, `resolve_flags`; `GET /tunnel/addresses`, `claim_error_response` (409 / 422 / 503 mapping), the daily stale warning. Shared by both peer registries. |
| `crates/wayhouse-controller/src/lease.rs` | `--tunnel-lease-ttl` expiry: `sweep` (list the book, `RegistryState::expire` each owner unseen past the cutoff, re-checked where the write lands), `lease_loop` (leader-only, 1 h startup grace), `check_ttl` (min 2 h). Under HA the write is `WriteRequest::Expire`. |
| `crates/wayhouse-controller/src/{peers,proxy_peers}.rs` + `{peers,proxy_peers}/api.rs` | The two mirrored registries (origins register in `peers`, proxies in `proxy_peers`): `POST` claims an address atomically with the registration (under a per-registry write lock), `DELETE` releases it and logs a tombstone, SSE `subscribe` replays registrations and tombstones. |
| `crates/wayhouse-agent/src/{register,address_store,proxy_subscribe,interface,keypair,main}.rs` | Origin agent: register first (bounded `http_client()`), `resolve_startup` picks controller answer vs saved `<data_dir>/tunnel-address`, `/32` proxy peers, `plan()`/`Action` for events and tombstones. |
| `crates/wayhouse/src/{proxy_register,tunnel_address,tunnel_client}.rs` (+ the `--tunnel-*` block in `main.rs`) | Proxy side of the same: register before bringing the interface up (before any listener binds), saved address at `<tunnel-key-file>.address`, `/32` origin peers, tombstones. |
| `crates/wayhouse-sniffer-abi/` | `wayhouse-sniffer-abi`, the guest-side ABI crate (workspace member; `wayhouse-ui` reads its ABI constants). The official sniffers are in `wayhouse-proxy/sniffers` and pin this crate by git revision; their releases are pinned here in `sniffers.lock` (`make sniffers-fetch`). |
| `crates/wayhouse-bench/` | `make bench` — `latency` mode (in-process, added p50/p99 vs. NFR N1/N2) + `concurrency` mode (real separate `wayhouse` process, connection-count ramp, `/proc` RSS/fd sampling). |
| `crates/wayhouse-fleet-tests/` | Phase 10+11 slice 12 (+ phase 14 `tests/tunnel.rs`, `#[ignore]`d, `make tunnel-e2e`, with `src/{netns,echo,tunnel}.rs` helpers): `cargo test -p wayhouse-fleet-tests` (part of `make check`) spawns real `wayhouse`/`wayhouse-controller`/`wayhouse-aggregator` binaries as child processes and drives them over real HTTP — controller reconnect/freeze/catch-up, reject-keeps-previous, aggregator push/ingest, fan-out partial failure. |
| `crates/wayhouse-config/fuzz/` | Standalone workspace: `extract_sni` / `route_match` / `parse_config` `cargo-fuzz` targets. `make fuzz` (nightly). |

---

## Testing

`make check` runs fmt + clippy `-D warnings` + `cargo test --all` (~520 tests across
`wayhouse-config`, `wayhouse-core` (unit + `crates/wayhouse-core/tests/{tcp_forward,udp_forward,amplification}.rs`),
`wayhouse` (incl. the `sniffer_loader` WAT-fixture end-to-end and the admin HTTP tests),
`wayhouse-controller` / `wayhouse-aggregator` / `wayhouse-ui` / `wayhouse-agent` (in-module HTTP and unit
tests) and `crates/wayhouse-fleet-tests`' real-multi-process tests (`fleet`, `gossip`,
`tunnel_addresses`; they debug-build and spawn the real binaries, adding real wall-clock
time). Needs `protoc` on `PATH`. Not part of `make check`:

- the namespace lab `make tunnel-e2e` / `make tunnel-e2e-ci` (12 scenarios, see "Known
  flakes & environment gotchas");
- `make ui-test` (vitest), `make deploy-lint` (daemon-free render checks of `deploy/`;
  needs the docker CLI + ruby), `make deploy-images` / `make deploy-smoke` / `make deploy-scan` (need a
  Docker daemon; the scan also needs `trivy`);
- the CI helper scripts: `python3 .github/scripts/changes_test.py` (needs `cargo`),
  `test_summary_test.py`, `trivy_summary_test.py`, `audit_summary_test.py` and `sarif_categories_test.py` (CI runs all five in the `changes` job).

CI runs Rust tests under `cargo nextest` (each test in its own process) — see
"Infra / environment".

- `TP­ROXY` / `IP_TRANSPARENT` e2e is not in CI (needs `CAP_NET_ADMIN`) — covered
  by config parse/reject + `connect_tcp_from` fallback tests. Setup recipe in
  `docs/04`.
- The UDP `prefix:` e2e needs a Linux host with `IP_PKTINFO` and loopback
  `127.0.0.2` / `127.0.0.3`; not portable to macOS/Windows CI.
- `#[ignore]`d, run in the `sniffers-e2e` CI job (not required) after `make sniffers-fetch`, on the
  official sniffers pinned in `sniffers.lock`: the ABI check, the `.wasm` artifact round-trip and
  the WASM-boundary N1 latency bench.
- `wasmtime` is a normal `cargo` dep — it does **not** need the
  `wasm32-unknown-unknown` rustc target; only the sniffers repo needs it, to *build*
  the sniffer crates. Loader tests assemble WASM from inline WAT via the `wat`
  crate (including the trap, timeout, memory-cap and ABI-version conformance cases).

---

## Infra / environment

- Toolchain via `rustup` (`stable`). If `cargo` isn't found:
  `export PATH="$HOME/.cargo/bin:$PATH"`.
- **`protoc` is a build requirement** (gRPC resolver codegen in
  `crates/wayhouse/build.rs`). CI installs `protobuf-compiler`.
- CI: `.github/workflows/ci.yml`. Docs-only pushes (`**.md`, `docs/**`, `LICENSE-*`) don't run it,
  and a newer push cancels an older run. A `changes` job (`.github/scripts/changes.py`, tested by
  `changes_test.py`, self-run in CI) decides which path-scoped jobs run from `cargo metadata`
  (a changed file's package plus its path-dependency dependents, against each job's `ROOTS`): `ui`, `sniffers`, `tunnel`,
  `deploy`, `fuzz`; `test` and `audit` always run for non-docs pushes, and `trivy` follows
  `deploy` (it scans `deploy`'s images, handed over as the 1-day `deploy-images` artifact
  of `docker save` tarballs). `trivy` and `audit` are informational: `continue-on-error`,
  results on the run's summary page, as warning annotations and as artifacts
  (`trivy-reports`, `cargo-audit`). A workflow edit, a failed diff, the
  **nightly schedule (03:17 UTC)** and `workflow_dispatch` run everything — so `deploy` and `fuzz`
  also act as nightly canaries for code-driven breakage. Adding a job, or changing which packages a job
  builds, means updating `ROOTS` in `changes.py` *and* its test; a new crate or dependency
  edge needs no edit. A full run bills roughly 60–80
  runner-minutes (each job rounds up to the minute; cold caches cost more); the scan
  jobs add under a minute each.
  Cargo caching is `.github/actions/cargo-cache` (rolling: a new snapshot per `main` push,
  restored by prefix `os-rustc-Cargo.lock`; PRs read it but don't write). It replaced
  `Swatinem/rust-cache`, which only saves on an exact-key miss — its key is the lockfile
  hash, so its snapshot froze at the last `Cargo.lock` change and a release-profile edit
  (`strip = true`) left `sniffers` recompiling all 342 crates every run. `fuzz` still uses
  `rust-cache` (nightly only, tiny). A lockfile or toolchain bump starts cold by design.
  Verified: a cold run took test 8m41 / build-release 10m53 / deploy 11m25; the next run
  restored the previous commit's snapshot (build-release recompiled 7 crates, 4m02, the rest
  is the thin-LTO link) and test took 3m53, deploy 1m22. Re-measured 2026-10-02 after the
  lockfile bump for `wayhouse-http`: cold `test` 8m15 / `build-release` 16.5 min, then warm
  `test` 4m41 / `build-release` 6m10 (see "Resume here").
  **Test reports:** CI runs Rust tests with `cargo nextest run --profile ci` (config:
  `.config/nextest.toml`; installed via a SHA-pinned `taiki-e/install-action`) and vitest with
  `--reporter=junit`; `.github/scripts/test_summary.py` (tested by `test_summary_test.py`,
  self-run in the `changes` job) renders the JUnit XML to the run's summary page and the XML is
  uploaded as a `junit-*` artifact. `make check` / `make tunnel-e2e` stay on plain `cargo test`
  (nextest optional locally); `make tunnel-e2e-ci` is the nextest variant. Ignored tests are
  not listed in the JUnit XML. **Nextest runs every test in its own process** — anything that
  assumed one shared process breaks (found: the tunnel lab's in-process namespace counter,
  fixed by probing for a free index in `Lab::add_ns`).
  **Shared release stage:** a `build-release` job compiles the five release binaries once
  (cache namespace `release`); `deploy` downloads the binaries as an artifact and builds the images with
  `BIN_SOURCE=prebuilt` (`deploy/Dockerfile`). Nightly `deploy` uses the self-contained
  in-Docker build instead, so that path can't rot. These three jobs are pinned to
  `ubuntu-24.04` (glibc 2.39): `ubuntu-latest` becomes Ubuntu 26 on 2026-10-19 and binaries
  built there might need a newer glibc than the distroless runtime's 2.41. (Since
  2026-10-02 every other job is pinned to `ubuntu-24.04` too.) `changes.py`
  emits a `release` flag (= the packages the sniffers e2e job covers, or deploy). Debug jobs (`test`, `tunnel`) deliberately
  do *not* share a build: tests hardcode `target/debug/<bin>` and `ensure_built()` runs
  cargo (mtime freshness would rebuild a downloaded artifact anyway), and with the rolling
  cache each only recompiles ~35 crates (~1 min).
- **Local verification tools** (none are in the repo): `cargo install cargo-nextest --locked`
  (for `make tunnel-e2e-ci`); `actionlint` (workflow lint; release binary from
  `github.com/rhysd/actionlint`, v1.7.7 used); `hadolint` (Dockerfile lint, v2.12.0 — its
  `DL3008`/`DL3006` warnings are expected); `kubeconform` (v0.6.7, the version CI
  downloads: `kubeconform -strict -summary deploy/k8s`). `docker compose ... config` renders
  the compose files without a daemon. The docs-only push filter means a pure `*.md` /
  `docs/**` change runs no CI; any other change runs at least `test`.
- git remote `github.com/wayhouse-proxy/wayhouse`, branch `main`; `git push`
  works, `origin/main` is current. The HTTPS credential helper logs a harmless
  "nonexistent Windows path" warning before falling back to a working credential.
- `Cargo.toml` declares `MIT OR Apache-2.0` with `LICENSE-MIT` / `LICENSE-APACHE`
  included. No explicit user decision on record — confirm if it matters.

---

## Open questions

Tracked in [#69](https://github.com/wayhouse-proxy/wayhouse/issues/69). Cross-instance
session failover is assumed **no** for v1 (ADR 4).
