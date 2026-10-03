# HANDOVER

Current state and the traps. This file is the *current-state + gotchas* layer only.
Design is the source of truth in [`docs/`](docs/); locked decisions are the ADR table
in [`docs/09-technology-choices.md`](docs/09-technology-choices.md). Per-slice
implementation history lives in `git log` and [`docs/08-roadmap.md`](docs/08-roadmap.md),
not here.

Last updated: 2026-10-03 (TLS handshake flood limits and bounded HA write forwarding; before that `--ca-file`, HA-over-TLS, native TLS on every server, security scanning in CI; see "Resume here").

## Current state

**All roadmap phases (0–14) are built, individually verified live, and covered by
`make check`.** No slice is in flight — the repo is at a natural stopping point.
Remaining work: the "Known follow-ups" table below. Every CI job blocks except the two
informational security scans, `trivy` and `audit` (2026-10-02, owner's call).

### Resume here (written for picking this up on another machine)

State at 2026-10-02 (latest): `--ca-file` (PR #1), TLS-capable HA peers + readable HTTP
errors (PR #2) and the cleanup PR (#3: one HTTP client for Raft RPCs, blocking
`tunnel`/`deploy`) and native TLS for `gsp-controller` (#4) are merged. Then, one PR each,
merged on green CI: native TLS for the aggregator (#5, with the shared
`gsp_http::tls::{TlsArgs, serve}`), the UI (#6) and `gsp`'s admin API (#7,
`settings.admin.tls`) — every fleet HTTP server can serve TLS itself; the deferred TLS
review minors plus two **informational** security-scan jobs, `trivy` and `audit` (#8);
and HANDOVER's CI/image follow-ups (#9). Nothing is in flight. The first real scans
(#8) were clean: no Trivy findings in the five images or the lockfiles, no `cargo audit`
vulnerabilities (three unmaintained crates, see "Known flakes").

**CI timings, measured.** Cold (lockfile changed; PR #1's run 37003396498): `test` 8m15,
`build-release` 16.5 min, each `tunnel` leg ~10 min, ~22 min wall clock. Warm (PR #2's run
37011410675, no lockfile change): `test` 4m41, `build-release` 6m10, `tunnel` 6m47 (kernel)
/ 8m12 (userspace), `plugins` 3m12, ~9.5 min wall clock. The rolling cache works; if a
later code push is back near the cold numbers without a lockfile change, the cache key in
`.github/actions/cargo-cache` is the first suspect.

Open decisions for the owner:

1. **Native TLS for the other HTTP servers:** decided and built (2026-10-02) — the
   controller, aggregator and UI via `--tls-cert`/`--tls-key`, `gsp`'s admin API via
   `settings.admin.tls`. New client code must not hard-code `http://` and must
   build clients via `gsp_http::{client, builder}`.
2. **Publish the reference images?** The owner chose "reference only" (2026-10-01).
   Publishing (GHCR on release tags, multi-arch if arm64 is needed) is a small follow-up:
   `deploy/Dockerfile` already has the `BIN_SOURCE` switch; it needs a release workflow,
   tags and a registry login. The owner also wants images built "for both Docker and
   Kubernetes" (2026-10-02) — the same OCI images already serve both, so this means
   publishing, multi-arch or k8s packaging; which one is still to be clarified.
3. **Self-hosted CI runner?** The owner has a spare VPS (6 cores, 12 GB RAM) and burned
   ~1,100 Actions minutes on 2026-10-02 (a heavy day: five full-pipeline PRs, several
   workflow edits that run every job, cold caches). Proposed (not done): a *hybrid* —
   one runner instance on the VPS (12 GB fits one Rust release build at a time) for
   `test`, `build-release`, `plugins`, with its persistent `target/` replacing the
   Actions cache; `tunnel` stays GitHub-hosted (it reconfigures netns/WireGuard on the
   host, and the 2026-10-01 self-hosted experiment below had a failing `tunnel` leg);
   `deploy`/`trivy` only if the VPS gets Docker for the runner user. Requirements:
   Ubuntu 24.04 / Debian 13 (glibc ≤ 2.41 for the release binaries), 60–80 GB free
   disk, a KVM VM, its own unprivileged user. Pending the VPS facts (virtualisation,
   OS, disk, what else runs on it). A self-hosted job just queues while the VPS is down
   — there is no fallback to hosted runners.

Never verified outside CI or the original dev sandbox:

- `make deploy-images` / `make deploy-smoke` and the compose **tunnel** override have not
  been run on a developer machine (the dev sandbox had no Docker daemon, so they were only
  ever exercised by the CI `deploy` job). On a machine with Docker, run both once.
- The k8s manifests were only schema-validated (kubeconform), never applied to a cluster.

Watch-list:

- **Runner image:** every CI job is pinned to `ubuntu-24.04` (2026-10-02), so the
  2026-10-19 `ubuntu-latest` → Ubuntu 26 switch changes nothing. Moving to 26 later is a
  deliberate edit of all jobs together: re-check `tunnel` (AppArmor userns sysctl,
  `wireguard` module) and the release jobs' glibc vs. the distroless runtime (2.41).
  GitHub supports a runner image for a while after it stops being `latest`, not forever.
- `actions/cache@v4` prints a Node 20 deprecation warning (it still works); bump when a
  Node 24 major exists.
- A self-hosted-runner experiment on 2026-10-01 (reverted in `79d1bd7`) had a failing
  `tunnel (userspace)` job whose log nobody read — cause unknown (host limits? sudo/apt on
  the VPS?). Only relevant if self-hosting is tried again — see open decision 3.
- **Merge queue** (owner asked about bors, 2026-10-02): PRs here land one at a time,
  each re-run on the latest `main`, so `main` only gets tested combinations. Once several
  PRs are in flight at once, GitHub's merge queue (needs an `on: merge_group` trigger;
  check availability for a personal private repo) would keep `main` green.

Suggested order: pick from "Known follow-ups" — candidates the owner raised: precise CI
change detection, the self-hosted runner (open decision 3), publishing the images (open
decision 2), and the deferred pieces of the address authority. Whether a red
`tunnel`/`deploy` also *blocks merging* depends on GitHub branch-protection required
checks, a repo setting outside this tree.

Most recent landings (newest first; full history in `git log`):

- gsp-ui tunnel addresses page (2026-10-03): `gsp-ui` proxies the controller's
  `GET /tunnel/addresses` as `GET /api/tunnel/addresses` (viewer role, the same
  `--controller-url`/`--controller-token` as the config API) and the frontend shows it
  on a read-only Tunnel addresses page: network and usage, one row per owner with
  address, role, first/last seen and a stale badge, plus how to free a stale address.
  Tests: `controller_proxy` `tunnel_addresses_proxies_to_the_controller_with_the_token`,
  the `header_contract` table, `TunnelAddressesPage.test.tsx`.
- HA write-forwarding timeout (2026-10-03): `HaHandle::forward` is one shared client
  (`ha::client::forward_client`) bounded by `FORWARD_TIMEOUT` (10 s, response body
  included), so a half-open leader costs the caller a `504` saying the write may still
  have been applied, instead of hanging the follower's write handler; other forward
  failures stay `503`. Pinned by `a_hung_leader_times_out_the_forward_instead_of_hanging_the_handler`.
  `ha/network.rs` now says its client relies on openraft's outer RPC timeouts.
- `cargo audit` in CI (2026-10-02, owner's request, ADR 28; informational): the `audit` job
  runs on every push/PR and nightly — RustSec advisories for the root, plugins and fuzz
  `Cargo.lock`s, no build needed. `cargo-audit` 0.22.2 comes prebuilt via the pinned
  `taiki-e/install-action`. Results like Trivy's: run-summary table
  (`.github/scripts/audit_summary.py`, tested in the `changes` job), a warning per
  vulnerable or unaudited lockfile, JSON in the `cargo-audit` artifact. It covers the
  RustSec-only advisories Trivy misses. Accepted advisories: `.cargo/audit.toml`.
- Trivy image scanning (2026-10-02, owner's request, ADR 28; **informational** by the owner's
  call): `make deploy-scan` (`deploy/scan-images.sh`), and in CI its own `trivy` job
  after `deploy`, which hands over its five images as `docker save` tarballs (artifact
  `deploy-images`, 1 day). The job runs the official `aquasec/trivy:0.75.0` image
  pinned by digest as a `docker://` step (not a job `container:` — JavaScript actions
  can't run in the Alpine image), with `continue-on-error`: it never fails. Results: a table on the run's
  summary page (`.github/scripts/trivy_summary.py`, tested by `trivy_summary_test.py`
  in the `changes` job), a warning annotation per affected or unscanned target, and
  JSON + SARIF in the `trivy-reports` artifact. Scope: each image's OS packages +
  secrets (`--input <tarball>` in CI; locally `--image-src docker`, never a same-named
  registry image), plus `Cargo.lock`
  and `crates/gsp-ui/web/package-lock.json` (a release binary has no embedded crate
  list); HIGH/CRITICAL with a fix. The image's `trivy` binary was checked byte-identical to the
  checksum-verified 0.75.0 release tarball — not `trivy-action`/`setup-trivy`, whose
  tags were hijacked in March 2026; re-check when bumping the digest. `.trivyignore` holds accepted findings (empty). A UI lockfile or
  `.trivyignore` change now also runs `deploy`.
- Native TLS review minors (2026-10-02): `ReloadingCert` judges a change by mtime, size,
  inode and ctime (a `cp -p`/`touch -r` rewrite is caught); a dead accept task is
  logged; `gsp-http`'s `tls` is behind a default-on `server` feature that the workspace
  dependency turns off (a standalone `cargo build -p gsp-agent` skips axum/rustls-server;
  the usual one-invocation build of every binary still unifies it on); tests now prove
  the chain is sent (`ca3`/`inter`/`leaf3` fixtures, with a leaf-only control), that
  ctime alone catches a same-length rewrite, a PKCS#1 RSA key, and that plain HTTP on
  the TLS port is closed. The two `first_party_*` sniffer tests get the latency bench's
  10 s `call_timeout` — the 50 ms epoch tick failed them now and then on CI.
  **Decided against** a cap on in-flight TLS handshakes: a global cap lets ~cap idle
  connects lock every client out (topped up every 10 s), worse than today's bound (one
  task + fd per stalled client until the 10 s timeout, as plain `axum::serve`). A real
  fix is per-source limiting or a shorter ClientHello timeout — see the follow-up row.
  *(Superseded 2026-10-03, next bullet: a global cap that evicts the oldest instead of
  refusing has no such lockout.)*
- TLS handshake flood limits (2026-10-03, ADR 29, spec
  `docs/superpowers/specs/2026-10-03-tls-handshake-limits-design.md`): `TlsListener`
  drops a client that hasn't sent its ClientHello within 3 s (`CLIENT_HELLO_TIMEOUT`;
  the whole handshake keeps 10 s), closes a source's 17th concurrent pending handshake
  (source = IPv4 address or IPv6 /64), and at 512 pending handshakes admits the new
  connection and aborts the oldest pending one. Constants in
  `gsp_http::tls::HandshakeLimits` (`bind_with`), no flags yet. A limit hit logs one
  `warn` per minute. Each accept now takes a short `std::sync::Mutex` twice (admit,
  register the abort handle) and each handshake task once more when it ends; never
  held across a spawn, abort or `.await`. No new task or hop.
- Native TLS for `gsp`'s admin API (2026-10-02): `settings.admin.tls: { cert, key }`
  (startup-only like `listen`; files renew every 30 s; `gsp --check` loads them; errors
  name `settings.admin.tls.cert`/`.key` via `TlsFiles::named`). The reported admin URL
  becomes `https://<listen>`, which the aggregator's fan-out reaches with `--ca-file`.
  E2E: `admin_native_tls.rs`.
- Native TLS for `gsp-ui` (2026-10-02): `--tls-cert`/`--tls-key`; serving HTTPS marks the
  session cookie `Secure`; `/ws/fleet` is routed with `any` because a browser on h2
  opens it as an extended `CONNECT` (RFC 8441, which `axum::serve` advertises with the
  `http2` feature) — `get` answered 405. Tests: `ws.rs`
  `an_http2_websocket_gets_the_current_view`, `ui_native_tls.rs`.
- Native TLS for `gsp-aggregator` (2026-10-02, spec
  `docs/superpowers/specs/2026-10-02-native-tls-other-servers-design.md`): the same
  `--tls-cert`/`--tls-key`; the flags and the HTTPS-or-HTTP choice are now shared as
  `gsp_http::tls::{TlsArgs, serve}`, which the controller uses too. E2E:
  `aggregator_native_tls.rs`.
- Native TLS for `gsp-controller` (2026-10-02, spec
  `docs/superpowers/specs/2026-10-02-controller-native-tls-design.md`, ADR 27, docs/12
  "Native TLS"): `--tls-cert`/`--tls-key` serve HTTPS through `gsp_http::tls::TlsListener`
  (handshakes in per-connection tasks, 10 s timeout); the certificate is re-read every
  30 s and a broken replacement keeps the current one. E2E: `controller_native_tls.rs`,
  and `ha_tls.rs` now also runs a 3-replica cluster on native TLS.
- Cleanup (2026-10-02): `ha::network::Network` holds one HTTP client, so Raft RPCs reuse
  their connection (`ha_tls.rs` counts terminator accepts: 26 new TLS connections per 5 s
  before, under 6 after); `tunnel` and `deploy` CI jobs are blocking; `CaError` names its
  cause once; measured CI timings recorded above.
- TLS-capable HA peers + readable HTTP errors (2026-10-02, spec
  `docs/superpowers/specs/2026-10-02-ha-tls-peers-design.md`, docs/12 "HA replicas over
  TLS"): `--ha-peers` entries may be `id=https://host[:port]`; `ha::peers::peer_url` builds
  every Raft RPC and forwarded-write URL. New `gsp-fleet-tests` `ha_tls.rs` is the first
  automated multi-replica HA test: three replicas reachable only through private-CA TLS
  terminators elect, forward and replicate with `--ca-file`, never elect without it.
  `gsp_http::error_chain` now renders every HTTP error with its cause (e.g.
  `invalid peer certificate: UnknownIssuer`). `/admin/adopt` never hard-coded `http://`
  (it takes a full `parent_url`) — the old follow-up row was wrong.
- Custom CA support (2026-10-02, spec `docs/superpowers/specs/2026-10-02-custom-ca-design.md`,
  ADR 26, docs/12 "gsp-controller behind TLS"): every binary takes `--ca-file <PEM>`,
  additive to the built-in Mozilla roots; a bad file is a startup error. New crate
  `gsp-http` is the one place production HTTP clients are built. `gsp-fleet-tests`
  `ca_file.rs` runs `gsp --check` through a private-CA TLS terminator (fails without the
  flag, passes with it). Test-only CA fixtures live in `crates/gsp-http/tests/fixtures/`.

- Tunnel address authority (2026-10-02, spec
  `docs/superpowers/specs/2026-10-02-tunnel-address-authority-design.md`, `docs/11`
  "Address authority"): `gsp-controller --tunnel-network` allocates/pins/releases
  tunnel addresses for origins and proxies; `gsp-agent` and `gsp --tunnel-*` register
  for an address before bringing the interface up (`--address` / `--tunnel-address`
  now optional), peers are `/32`s. This fixed the old "second proxy steals the first
  one's route" bug (`two_proxies_share_one_origin` passes in the lab). The tunnel e2e
  lab has 12 scenarios, green on both backends. With the controller down,
  `gsp --tunnel-*` spends its ~30 s registration budget before falling back to the
  saved address.

- `deploy/` — reference `Dockerfile` (five distroless targets), compose
  control-plane demo + tunnel override, plain k8s manifests, `deploy/smoke.sh`, CI job
  `deploy` (blocking since 2026-10-02). **First CI run (36922142697, 2026-10-01) was
fully green**: all five images built and ran `--version`, the compose smoke passed
end to end, kubeconform passed. Not exercised anywhere: the tunnel override and the
k8s manifests on a real cluster. A pre-push fresh-context review caught one bug that
would have failed that first run (curl 8.11.1 rejects `--opt=value`, so the `seed`
args are separate); `make deploy-lint` pins that and other render-time facts.
- `docs/12-deployment.md` — container image sizing + the Docker/Kubernetes
  port-exposure model (host networking vs. a pre-reserved `bind: "host:lo-hi"`
  range; `CAP_NET_ADMIN` / `/dev/net/tun` for `--tunnel-*` / `gsp-agent`).
- Phase 14 slice 7 — proxy-peers registry, so an origin's `gsp-agent` learns every
  proxy (added or pre-existing) without an origin-side restart.
- Phase 14 slice 6 — end-to-end live verification in 4 Docker containers; fixed two
  real bugs (`gsp-agent` never added the edge proxy as a peer; `reconcile_peer` tore
  down and rebuilt the WireGuard session on every unchanged re-registration).

## What's built

### Data plane — `gsp` + `gsp-core` + `gsp-config` (phases 0–9 + two follow-on passes)

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
- Sniffer plugins via `wasmtime` — no game-protocol code in core. First-party
  `a2s` / `minecraft` / `regex-firstbytes` plugins in `crates/plugins/`
  (standalone workspace, `make plugins`). Per-plugin config, benchmarked p50 ~8–10 µs.
- Perf pass: `splice(2)` zero-copy TCP pump, `recvmmsg(2)` UDP ingress batching,
  single-level timing-wheel UDP idle expiry.
- Ops: `gsp_build_info{version,commit}`, `gsp_fd_open` / `gsp_fd_limit` sampling.

### Distributed control plane — phases 10–13 (fully built)

- **`gsp-controller`** — Tier-1 config + operator-intent distribution. `sled`-backed
  revision logs, `GET /config/subscribe` (SSE) catch-up-then-tail,
  revisions / diff / rollback, optional `--auth-token` bearer gate. Fleet hierarchy
  (`standalone` / `slave` relay `--role`), post-install adoption
  (`POST /admin/adopt`), intra-tier HA (embedded `openraft`, real 3-node cluster,
  transparent leader HTTP-forwarding — clients stay HA-unaware), staged/canary
  rollout (`?stage=canary&group=`, `POST /config/promote/{rev}`), per-revision
  `actor` audit field.
- **`gsp-aggregator`** — leaf→root fleet-state push (`POST /ingest` →
  `GET /fleet/pools|sessions|healthz|subscribe`), aggregator-of-aggregators
  hierarchy (namespaced `tier/instance`), intent-verb fan-out (targeted
  drain/undrain + broadcast backend/route-hint edits), `--auth-token` /
  `--instance-token` gates. No `gsp-core`/`gsp-config` dep; carries no durable state.
- **`gsp-ui`** — dedicated BFF process (not hosted in controller/aggregator).
  Session-cookie auth, proxies both upstream APIs forwarding response headers,
  `GET /ws/fleet` live feed off one shared aggregator SSE subscription, RBAC
  (`viewer` < `operator` < `admin` via `--users-file` argon2 hashes; legacy
  single `--ui-password` kept). React + Vite + TS frontend in `crates/gsp-ui/web/`
  (`make ui`, `--static-dir`) — redesigned in `d86c786` (Tailwind + Radix,
  react-router, grouped fleet tree via `settings.group`, schema-driven settings
  form over a raw-YAML escape hatch, Plugins page backed by `GET/POST/DELETE
  /admin/sniffers`). Destructive actions (drain, remove backend, set a backend
  `draining`/`disabled`, rollback, plugin remove) confirm first via
  `useConfirm()`; action buttons disable while a request is in flight; a
  vitest suite (`make ui-test`, CI `ui` job) pins that, and
  `gsp-ui`'s `header_contract` test pins that every proxied route forwards
  status + response headers (**add new proxied routes to its `ROUTES` table**).
- **Tier-2 regional health gossip** — embedded `foca` SWIM mesh over one HMAC-auth
  UDP socket, per-backend last-writer-wins health broadcast piggybacked on foca's
  own anti-entropy, quorum-based `Backend::domain_down` override (additive to the
  local `healthy` flag; only a local `rise` streak clears `healthy`). Spawned only
  when `settings.gossip` is set.
- **`crates/gsp-fleet-tests`** — spawns the real `gsp` / `gsp-controller` /
  `gsp-aggregator` binaries as child processes over real HTTP; part of `make check`
  (adds ~15–20 s).

### Backend transport — phase 14 (`docs/11`, ADR 25)

WireGuard tunnels so a globally-distributed proxy fleet can reach game servers that
are **not** on a shared trusted network. Unmodified WireGuard is the entire tunnel
data plane (kernel module primary, `boringtun` userspace fallback). All 7 slices
built; verified live in 4 Docker containers (`--cap-add=NET_ADMIN
--device=/dev/net/tun`).

- **`gsp-agent`** — origin-side binary (own workspace crate). Brings up one local
  WireGuard interface via `defguard/wireguard-rs`, persists a stable keypair
  (`<data_dir>/private.key`, `0600`, never rotated by this code), registers its
  pubkey + fronted backend addresses with the controller's backend-peers registry
  on a fixed interval, and subscribes to the proxy-peers registry to reconcile every
  proxy onto its interface.
- **`gsp-controller`** — two registries alongside the config/intent logs, each its
  own `sled` db: `peers` (backend-peers — origins register, proxies subscribe) and
  `proxy_peers` (the mirror — proxies register, agents subscribe). `POST` / `GET` /
  `GET …/subscribe` on each; validated (pubkey base64-decodes to 32 bytes, backends
  parse as `SocketAddr`) before the store.
- **`gsp` `--tunnel-*`** — brings up the shared WireGuard interface *before* any
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

## Deferred / not built

- **CGNAT / both-sides-behind-restrictive-NAT** (phase 14) — documented v1
  limitation. A relay-of-last-resort (closer to Steam Datagram Relay's shape) is a
  possible v2, not designed.
- **`gsp-ui` leftovers** — config *submit* (Settings page) still applies without a
  confirmation step, and there is no end-to-end browser test (Playwright) — only
  component tests against a mocked `api.ts`.
- **Tunnel e2e leftovers** (deferred minors from the 2026-10-01 branch review; none
  affect correctness of what is asserted today):
  - the 1200-byte UDP check in scenario 1 is one datagram with no retry — a single
    dropped datagram on a fresh path would flake it (a 3-try loop fixes it);
  - scenario 4's negative check looks for `/pools` lines starting with two spaces; it
    would pass vacuously if that format changed — also assert the pool name is present;
  - `echo.rs`: if `setns` fails inside the thread, the error surfaces as "did not
    report ready within 5s" instead of the real cause;
  - a blocked user namespace (e.g. Ubuntu AppArmor without the CI `sysctl`) makes
    `unshare -Urnm` fail in the Makefile *before* the in-test hint can name
    `make tunnel-e2e`;
  - `make tunnel-e2e` also builds `gsp-aggregator`/`gsp-ui` (unused by these tests, build
    time only), and `ensure_built()` runs `cargo build` again inside the namespace, which
    works offline only because everything is already built (and it recompiles `ring` there
    on every run — cause not investigated);

- **HA + `--role slave` together** — rejected at startup today. Needs the upward
  relay to run leader-only with its cursor promoted to replicated state (designed in
  `docs/10`, not built).
- **`sendmmsg` UDP egress batching**, k8s discovery watch informer, resolver
  `sticky_key` / `sticky_key` recovery, per-domain gossip capacity/load signals,
  `failure_domain` auto-discovery — see the table below.

## Known flakes & environment gotchas

- **Disk fills up in long sessions.** `target/debug` grew to ~30 GB over many test
  builds on 2026-10-02 (the linker then fails with exit 1, not a clear "no space");
  `target/debug/incremental` alone was 12 GB. `rm -rf target/debug/incremental` is the
  cheap fix, `rm -rf target/debug` the full one (one cold rebuild).
- **`make deploy-lint` and the locale.** Its ruby checks read the Dockerfile as
  US-ASCII under a `C`/POSIX locale and fail with `invalid byte sequence`; run it with
  `LANG=C.UTF-8 LC_ALL=C.UTF-8` (CI's runners are UTF-8 already).
- **`make audit`** runs `cargo audit` over all three lockfiles (root, `crates/plugins`,
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
  until the listener is up. `gsp-controller`'s `seeded_entries_are_readable_and_persist_across_a_reopen`
  reopened `sled` immediately after dropping it (the file lock is released on a
  background thread); it now retries the open. **The other ~30 `sleep(150ms)` +
  connect tests in `crates/gsp-core/tests/tcp_forward.rs` have the same shape** —
  if one flakes, reuse `connect_when_listening` there rather than lengthening the sleep.
- **A config file directly under `/tmp`** triggers continuous ~200 ms
  `configuration reloaded source=file` log spam (a `notify` / tmpfs
  mtime-granularity interaction). Harmless — `reload.rs` only swaps the `Snapshot` —
  but run live tests from a real directory; a real deployment's config isn't on tmpfs.
- **Rust handler ↔ TS frontend wire-shape mismatches** are a recurring bug class
  (hit ≥3×: a pass-through proxy silently dropping every upstream response header;
  `JSON.parse("ok")` on a text body). A thin proxy needs an explicit test that a
  header survives the hop — status + body assertions don't catch it.
- **`shutdown_grace_sec: 30`** in `config.example.yaml` means `timeout N gsp` does
  not kill at `N` (SIGTERM starts a graceful drain that can run the full 30 s). Use
  `timeout -s KILL` and give helper processes a longer lifetime than the proxy in
  smoke scripts. Bare `kill`/`pkill` on background test processes has also produced
  a stray "Exit code 144" from the Bash tool in this environment — wrapping each
  process in `timeout -s KILL` instead avoids it. **Never `pkill -f <pattern>` from an
  agent shell:** the pattern also matches the shell's own command line and kills it; use
  `pkill -KILL -x <exact-process-name>` (`gsp`, `gsp-controller`, `gsp-agent`, …).
- **`--tunnel-*` / `gsp-agent` need `CAP_NET_ADMIN` + `/dev/net/tun`** (even
  `boringtun` userspace does, for the TUN device). In containers grant both
  (`docs/12`). The rootless tunnel e2e lab gets `CAP_NET_ADMIN` from a user namespace
  (`unshare -Urnm`), so it runs without root or Docker — including in the original dev
  sandbox. `TunnelSource` origin-name matching: `backend_sources[].name` must equal the
  origin's `gsp-agent --name` (intentionally the same string).
- **Phase 14 tunnel e2e** — `make tunnel-e2e` (`crates/gsp-fleet-tests/tests/tunnel.rs`,
  CI job `tunnel`, kernel + userspace matrix) runs the real `gsp-controller` /
  `gsp-agent` / `gsp --tunnel-*` binaries in rootless network namespaces
  (`unshare -Urnm`; no Docker, no root). It replaced the one-off Docker harness.
  The CI job is **blocking** since 2026-10-02. It is green on both backends on GitHub (`unshare -Urnm`
  + tmpfs on `/run` and the `wireguard` module work on the Ubuntu runners). The lab now has
  **12 scenarios** (namespace helpers, TCP/UDP round trip, stays-up-across-re-registrations,
  a proxy added later, **two proxies sharing one origin**, a pinned-key mismatch, a pinned
  address collision, an edge restart keeping its address, an edge restarting with the
  controller down) and takes ~4–5 min per backend locally, ~9.5 min per leg in CI (the
  kernel edge-restart scenario alone waits up to 200 s). Run it with
  `TUNNEL_BACKEND=kernel|userspace make tunnel-e2e` (plain `cargo test`) or
  `make tunnel-e2e-ci` (nextest + JUnit; needs `cargo install cargo-nextest --locked`). Needs `unshare`, `ip`,
  `nsenter`; the userspace backend also needs `/run/wireguard`
  (the make target mounts a tmpfs on `/run` for it). Traps it taught:
  **`/pools` health is optimistic** (a new backend is `healthy` before the tunnel is
  up — wait for a real round trip); **userspace (`boringtun`) first handshake takes
  ~25 s** (the proxy has no endpoint for the origin, so it waits for the agent's
  25 s persistent keepalive; kernel is ~2 s) — observed 2026-10-01, not fixed;
  dead namespaces' veths disappear asynchronously, so test namespaces never reuse
  names within a run. Slice 7 (proxy-peers registry) is live-verified, including two
  proxies carrying traffic at once.

## Known follow-ups (none blocking)

| Item | Notes |
|------|-------|
| Tunnel address authority — deferred pieces (decided out of scope 2026-10-02, owner wants them later) | Spec: `docs/superpowers/specs/2026-10-02-tunnel-address-authority-design.md`: **IPv6** tunnel networks; **HA-replicated allocation** (the registries aren't Raft-integrated, so `--tunnel-network` + `--ha-peers` is refused at startup); **automatic lease expiry** (v1 is explicit release + a stale warning); releasing an address **from gsp-ui** (its Tunnel addresses page is read-only; release is the registry `DELETE`); **changing a live peer's address without a restart** (v1 logs the mismatch and keeps running); `TunnelSource` **dropping pool entries when an origin is deleted** (a `404` still means "keep last-known-good") |
| Address authority — deferred review minors | `warn_stale` is silent on a storage error; `allocate()` is an O(allocated) scan under the global mutex and `allocated()`/`Exhausted` use `Tree::len()` (O(n)) — consider capping `--tunnel-network` size; `check_pin` treats an unparseable holder as free; `parse_duration` can overflow (use `checked_mul`); stored addresses are not re-validated if `--tunnel-network` later changes; a store failure after a successful claim also keeps the claim (the doc comment only mentions the backend-422 case), and a stream of distinct names with bad backends can use up the pool (bearer-gated; DELETE + the stale warning are the remedy); every 4xx is treated as a permanent registration failure incl. 408/429 (consider transient); a changed `--address` pin loses to the saved address on a transient failure without notice; the final transient error is not logged when falling back to the saved address; a name re-registered with a NEW pubkey never removes the old key's peer (pre-existing); `the_production_client_has_a_request_timeout` waits ~10 s; lab: scenario 8 does not assert the edge came up ON its saved address, `agent_refused` loses the agent log on timeout, `start_controller` drops failed attempts' logs and its sled-lock comment may be wrong, scenario 7 asserts stickiness only after the 200 s wait, `restart_edge` has a redundant sleep and deletes the shared boringtun socket path (safe only for single-edge scenarios) |
| Kernel WireGuard: a restarted edge `gsp` leaves the tunnel down for ~2.5 min (found 2026-10-02; pre-dates the address work) | The edge has no endpoint for the origin so it cannot start a handshake; the agent sees an identical proxy registration so never re-sets the peer; keepalives do not re-key a session it still believes valid; recovery waits for WireGuard's 120 s rekey. A possible fix is a boot id in the proxy registration (protocol change), not done. The lab's restart scenario therefore waits up to 200 s after an edge restart (`wait_roundtrip_after_restart`), which adds ~2.5 min to the kernel `tunnel` CI leg. |
| Native TLS — coarse timestamps / handshake limit follow-ups | `ReloadingCert`'s stamp misses a same-length, same-inode rewrite within one tick of the last load on a coarse-timestamp filesystem (ctime is as coarse as mtime there) — caught by the next change. CI never builds `gsp-http` with `--no-default-features` (a `cargo check -p gsp-agent` step would). Handshake limits (2026-10-03) leave out: a per-source *rate* of new connections (a source can cycle connects under its cap), flags for `HandshakeLimits`, and metrics for refused/evicted handshakes (`gsp-http` has no metrics registry). |
| Trivy scan — follow-ups | (1) GitHub's Security tab ("code scanning") would show the SARIF natively, but this repo is private, so uploads need GitHub Code Security (paid); with it, add `github/codeql-action/upload-sarif` (permission `security-events: write`) over `target/trivy/*.sarif`. (2) For Rust crates Trivy sees GHSA advisories only (RustSec-only ones such as the 2026-10-01 rustls/wasmtime fixes are missed, and many crate advisories are MEDIUM) — `cargo audit` (the `audit` job) is the real check. (3) `cargo auditable` builds would let the image scan see the crates itself. (4) The vulnerability DB (~120 MB, `mirror.gcr.io` with a `ghcr.io` fallback) is fetched each run — cache it by day. (5) The pinned hash stops a later swap but can't prove 0.75.0 was clean when pinned; verifying the release's cosign/sigstore bundle would. (6) `package-lock.json` scanning includes build-only `dependencies` (tailwind, vite via `@tailwindcss/vite`), so a dev-server CVE there would be a false positive. (7) Cosmetic: `trivy convert --format table` in `scan-images.sh` logs "No enabled scanners found" and prints no table to the job log (seen on the first CI run, 2026-10-02); the run-summary table and the reports are unaffected — pass the scanners to `convert` or drop the log table. |
| Publish the reference images | Reference-only today (owner's choice). GHCR on release tags (+ multi-arch if arm64 is needed): a release workflow, tags and a registry login; `deploy/Dockerfile`'s `BIN_SOURCE` switch already supports building from CI-built binaries. Owner (2026-10-02): images should be built "for both Docker and Kubernetes" — not yet specified whether that means publishing, multi-arch or k8s packaging (Helm/Kustomize); ask. Once images are published, per-image CI jobs make sense (each versioned, rebuilt and pushed only when its inputs change); before that they don't — see the CI-cost row. |
| CI: precise change detection | `.github/scripts/changes.sh` maps paths to CI areas with hand-written regexes, which drift as crates gain dependencies. Deriving the affected binaries from `cargo metadata` (reverse deps of the changed crates) would let jobs run only for what changed — e.g. no release build when only `crates/gsp-ui/web/` changed. Discussed 2026-10-02 against per-container jobs: the five binaries share most of their compile (one `cargo build` links all five), image builds copy prebuilt binaries (seconds), the compose smoke test needs all five together, and per-image Trivy jobs would fetch the ~120 MB DB five times — so five parallel jobs per stage would cost more billed minutes, not fewer. Related: cache the Trivy DB by day (Trivy row). |
| Change a live HA member's address | `--ha-peers` only bootstraps a cluster; each member's address then lives in the Raft membership, so an existing `host:port` cluster cannot move to `https://` peers (or to new hosts) by editing the flag. Needs openraft's membership-change API plus an operator verb (docs/10 already lists dynamic membership as deferred). Workaround today: bootstrap a new cluster. |
| HA-over-TLS — deferred review minor (2026-10-02) | `error_chain` dedups by substring (documented trade-off, could hide a short source contained in an earlier message). The other minors of this row were fixed in the cleanup PR (one client for Raft RPCs, docs/10 wording, a self-standing negative test). |
| HA-over-TLS cleanup-PR minors (found reviewing the cleanup PR, 2026-10-02) | `tls_front_counted` counts TCP accepts though docs/messages say "TLS connections"; the negative HA test's `contains("certificate")` is loose (`unknownissuer` alone would be tighter); cold `build-release` varied 11–17 min across measured runs |
| `--ca-file` — deferred review minors (2026-10-02) | no test sets `--ca-file` against a plain `http://` endpoint (correct by construction); `crates/gsp-http/tests/fixtures/leaf.key` may need a secret-scanner allowlist entry if one is ever enabled. (Fixed since: the docs/12 stray `: `, the cause printed twice in `CaError`, the `format!` log field.) |
| `gsp` aggregator `admin_url` override | `gsp --aggregator-*` reports `admin_url` = `http://<settings.admin.listen>` with no flag to override, so aggregator intent fan-out cannot reach a containerised/k8s `gsp` (found reviewing `deploy/`); needs e.g. `--aggregator-admin-url` |
| `sendmmsg` UDP egress batching | reply pump + upstream forward still one `send` per datagram; per-session reply buffers of `RECV_BATCH`×`MAX_DATAGRAM` would 16× RSS — needs a smaller batch buffer or per-datagram alloc, its own decision |
| Per-source cap + UDP sticky table: LRU eviction | both refuse / wholesale-clear when full today; acceptable defaults — do only if load testing shows them biting |
| k8s discovery watch informer | polling Endpoints now; a convergence-speed optimization, belongs with the fleet-phase discovery rework |
| Resolver `sticky_key` | deferred pending a design for how a later request recovers the key; overlaps the phase-11 intent model |
| `IPV6_TRANSPARENT` on musl / non-glibc | `set_ip_transparent` already calls `socket2` 0.6's `set_ip_transparent_v6` unconditionally — may already work; build + smoke-test on a musl target before writing code |
| Retire the UDP sticky table via `consistent_hash` | pure polish, no user-visible gap |
| Reload debounce only coalesces within one 200 ms window | wider-spaced events cause separate (idempotent) reloads; low priority |
| NFR N3/N4/N5/N9 (aggregate throughput, full 500k/1M, HA) | need dedicated hardware + multiple hosts + a real load generator; the `gsp-bench --mode concurrency` ramp already went as far as one box allows (~20k TCP / ~5k UDP verified here) |
| More sniffers (`quic`, `wireguard`, …), multiple sniffers per listener | community / plugin ecosystem; never blocks core work |

---

## Workflow gotcha: run `cargo fmt --all` as its own step before `make check`

CI once failed a `cargo fmt --all --check` even though `make check` had reportedly
passed locally, because `cargo fmt --all` was run early in the session and more
code was `Edit`ed in afterwards. `--check` (which `make check` runs) only
*reports* diffs and exits non-zero — it never rewrites the file. So:

**Immediately before every `make check` / commit, run `cargo fmt --all` (the
writing form, no `--check`) as its own step** — never assume an earlier fmt pass
covers later edits. Piping `make check` through `tail` also hides an early
`fmt-check` failure message; check the exit code or read from the top.

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
  live. A `ListenerConfig` change (bind, protocol, routes, affinity, `prefix`,
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
  reply. Covered by `crates/gsp-core/tests/amplification.rs`.

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
  lock, no alloc, no task spawn. Reply path is still one `recv` + one
  `send`/`send_to` per datagram (`sendmmsg` egress deferred).
- Recv buffers: `RECV_BATCH` (16) × 64 KB **per worker** (shared across that
  worker's sessions) + one 64 KB buffer per reply task.
- Established UDP sessions skip the ACL / rate-limit / geo / gate checks
  entirely — those run only for datagrams that miss the session table.

### Per new TCP connection / new UDP session (paid once)

- **Base**: 1 `Pool::acquire[_for]` (lock-free reads + one atomic add), 1
  upstream `connect`, 1 spawned pump/reply task, (UDP) 1 socket `bind`+`connect`
  + sticky-table + session-table insert.
- **Routing**: 1 `local_addr()` syscall + a linear scan of the small route list
  (bit-compare per `client_cidr`/`dst`, `u16` range per `port`, `starts_with` +
  len check per `first_bytes`, one `extract_sni` pass per `sni`).
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
- **Sniffer plugin** (`WasmSniffer`): one `Store::new` + `Instance::new` (fresh
  per call) + `memory.write` of the peeked bytes + one guest call + decode, all
  synchronous on the per-conn task. Bounded by `call_timeout_ms` (epoch
  interruption) + `max_memory_bytes`. **Benchmarked**: p50 8–10 µs, p99 12–26 µs
  for the three first-party plugins — comfortably inside NFR N1 (500 µs), so the
  fresh-`Store`-per-call design needs none of the `InstancePre` / pooling /
  warm-instance fallbacks held in reserve. Listeners without a `sniffer:` route
  pay one `HashMap::get`. See `docs/07` "Sniffer plugin sandbox guarantees".
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
| `crates/gsp-config/src/lib.rs` | Raw YAML types, `validate()`, resolved `Config` / `PoolConfig` / `ListenerConfig` / `ResolverConfig` / `SniffersConfig` / `HealthCheck`; routing (`Matcher`, `Action`, `OnError`, `Cidr`, `CidrSet` trie, `HostPattern`, `MatchContext`, `RouteHint`, `extract_sni`); filters (`Acl`, `GeoAcl`, `RateLimit`, `PerSourceLimit`, `GlobalLimits`). **All schema rules here.** |
| `crates/gsp-core/src/snapshot.rs` | `Snapshot { listeners, pools, sources, resolvers, limits, geo_db }`; `build` → `build_with_overlay` → `build_with_sources` carry health/admin-state over by address and apply overlay + discovered backends. |
| `crates/gsp-core/src/pool.rs` | `Pool` (balancer, `rr` index, `hash_on`, `weights`; `acquire` / `acquire_for` / `acquire_addr` / `backend`, `hrw_score`), `Backend` (health / active / streaks / `check_kind` + `AdminState`), `BackendGuard` (RAII slot + passive health), `PickError`. |
| `crates/gsp-core/src/listener.rs` | `run_tcp_listener`: accept loop; per-conn task does ACL/geo/rate/`per_source`/global-cap checks, first-bytes peek, route match, pool lookup. |
| `crates/gsp-core/src/listener_udp.rs` | `run_udp_listener`: per-worker `recvmmsg` batch loop, `(client, Option<SocketAddr> dst)` session table, sticky affinity, `IdleWheel` idle expiry, per-session upstream socket + reply pump. `UdpMode` Plain / Prefix (`IP_PKTINFO` + `sendmsg` reply) / Transparent (`IP_ORIGDSTADDR`, client-bound upstream, per-session `IP_TRANSPARENT` reply socket). |
| `crates/gsp-core/src/{ratelimit,src_conns,limits,geo}.rs` | Per-listener token bucket / per-source concurrent cap / process-wide caps / MaxMind country lookup. |
| `crates/gsp-core/src/sniff.rs` | `Sniffer` trait + `Sniffers` `ArcSwap`-backed registry (`register` / `get` / `replace`) + `warn_if_missing`. **No built-in sniffers** — the seam the phase-9 loader fills. |
| `crates/gsp-core/src/route_hint.rs` | `RouteHints` — `ArcSwap<HashMap>` `src_ip → pool` push-resolver table (`POST /route-hint`), lock-free read. |
| `crates/gsp-core/src/resolver.rs` | `trait Resolver`, `ResolveRequest` / `Resolution` / `ResolveError`, `Resolvers` (`ArcSwap`-backed) map, `resolve_route` (async route walk), `CachedResolver` (TTL LRU). Transports live in `gsp`. |
| `crates/gsp-core/src/discovery.rs` + `sources.rs` | `trait BackendSource` + `Discovery` last-known-good cache + `refresh_loop`; `SourceManager` reconciles one refresh task per pool `source` on reload (discovery analogue of `ListenerManager`). |
| `crates/gsp-core/src/drain.rs` | `ConnTracker` / `ConnGuard` — `watch<usize>` live count + an `id → {proto, listener, peer, local, pool, backend, since}` registry (`GET /sessions`); `wait_idle()`. |
| `crates/gsp-core/src/overlay.rs` | `BackendOverlay` — runtime backend add/remove, layered on file `targets` at rebuild. |
| `crates/gsp-core/src/proxy.rs` | `handle_tcp` (pool) / `handle_tcp_target` (resolver `target`, no guard) → `connect_backend` + `pump` (`splice` / buffered). Writes the PROXY protocol header before the pump. |
| `crates/gsp-core/src/proxy_protocol.rs` | `header(mode, src, dst)` — PROXY protocol v1 (text) / v2 (binary, STREAM or DGRAM). Write-only. |
| `crates/gsp-core/src/health.rs` | 500 ms sweep, probes due backends (`tcp_connect` / `udp_probe`), updates health + `gsp_pool_backends` gauges. |
| `crates/gsp-core/src/runtime.rs` | `Runtime::start*` builds `ListenerManager` + `SourceManager` + health task; owns `RouteHints` / `ConnTracker` / `BackendOverlay` / `Sniffers` / `reload_requested`. `shutdown_with_grace` = stop listeners + await health + `wait_idle` + `abort_all`. |
| `crates/gsp-core/src/listeners.rs` | `ListenerManager` — one task `Group` (workers + `watch<bool>` stop) per listener; `start_all`, `reconcile(&Snapshot)` (diff by name), `stop_all` / `abort_all`. |
| `crates/gsp-core/src/net.rs` | `bind_reuseport_tcp` (+ `freebind` / `transparent`), `bind_reuseport_udp` (`UdpMode`), `bind_transparent_udp`, `connect_tcp_from`, `set_ip_transparent` (v4 + v6). |
| `crates/gsp-core/src/metrics_defs.rs` | **Every metric name** (`pub const`). |
| `crates/gsp/src/main.rs` | CLI (`--config`, `--check`), tracing, runtime bring-up, shutdown. Opens `geo_db` + builds sniffers (fails `--check` / startup on a bad one). |
| `crates/gsp/src/admin.rs` | axum router: `GET /healthz` `/readyz` `/metrics` `/pools` `/config` `/sessions`; `POST /route-hint` `/admin/drain` `/admin/undrain`; `PATCH` backend state; `POST` / `DELETE` a backend. `router(state)` split out for the in-module HTTP tests. |
| `crates/gsp/src/resolver.rs` | `HttpResolver` (`reqwest`), `GrpcResolver` (`tonic`, `mod pb` from `build.rs`), `build_resolvers`. |
| `crates/gsp/src/sniffer_loader.rs` | `SnifferLoader` (shared `wasmtime::Engine` + epoch-ticker thread) + `scan(&SniffersConfig)`; `WasmSniffer`; `build_sniffers` = `new` + one `scan`. |
| `crates/gsp/src/discovery.rs` | `DnsSrvSource` (`hickory-resolver`), `ConsulSource` / `KubernetesSource` (`reqwest`), `DiscoveryFactory`. |
| `crates/gsp/src/reload.rs` | `SIGHUP` + `notify` file watch + `reload_requested()` → debounce → `apply` (validate, `build_with_overlay`, store, reconcile listeners / sources / resolvers, rescan sniffers). |
| `crates/gsp/src/procinfo.rs` | `gsp_build_info` / `gsp_fd_open` / `gsp_fd_limit` — build identity + a small detached `/proc/self/fd` sampling task. |
| `crates/gsp/proto/resolver.proto` + `build.rs` | gRPC resolver contract + `tonic_build` codegen (needs `protoc`). |
| `crates/gsp-controller/src/addresses.rs` + `addresses/api.rs` | Tunnel address authority: `Network`, `AddressBook` (claim / release / entries over two sled trees, one mutex, flush after each transaction), `expand_backends`, `resolve_flags`; `GET /tunnel/addresses`, `claim_error_response` (409 / 422 / 503 mapping), the daily stale warning. Shared by both peer registries. |
| `crates/gsp-controller/src/{peers,proxy_peers}.rs` + `{peers,proxy_peers}/api.rs` | The two mirrored registries (origins register in `peers`, proxies in `proxy_peers`): `POST` claims an address atomically with the registration (under a per-registry write lock), `DELETE` releases it and logs a tombstone, SSE `subscribe` replays registrations and tombstones. |
| `crates/gsp-agent/src/{register,address_store,proxy_subscribe,interface,keypair,main}.rs` | Origin agent: register first (bounded `http_client()`), `resolve_startup` picks controller answer vs saved `<data_dir>/tunnel-address`, `/32` proxy peers, `plan()`/`Action` for events and tombstones. |
| `crates/gsp/src/{proxy_register,tunnel_address,tunnel_client}.rs` (+ the `--tunnel-*` block in `main.rs`) | Proxy side of the same: register before bringing the interface up (before any listener binds), saved address at `<tunnel-key-file>.address`, `/32` origin peers, tombstones. |
| `crates/plugins/` | Standalone workspace (own `[workspace]`): `gsp-sniffer-abi` guest helper + `a2s` / `minecraft` / `regex-firstbytes` plugins. `make plugins`. Never a dep of `gsp` / `gsp-core`. |
| `crates/gsp-bench/` | `make bench` — `latency` mode (in-process, added p50/p99 vs. NFR N1/N2) + `concurrency` mode (real separate `gsp` process, connection-count ramp, `/proc` RSS/fd sampling). |
| `crates/gsp-fleet-tests/` | Phase 10+11 slice 12 (+ phase 14 `tests/tunnel.rs`, `#[ignore]`d, `make tunnel-e2e`, with `src/{netns,echo,tunnel}.rs` helpers): `cargo test -p gsp-fleet-tests` (part of `make check`) spawns real `gsp`/`gsp-controller`/`gsp-aggregator` binaries as child processes and drives them over real HTTP — controller reconnect/freeze/catch-up, reject-keeps-previous, aggregator push/ingest, fan-out partial failure. |
| `crates/gsp-config/fuzz/` | Standalone workspace: `extract_sni` / `route_match` / `parse_config` `cargo-fuzz` targets. `make fuzz` (nightly). |

---

## Testing

`make check` runs fmt + clippy `-D warnings` + `cargo test --all` (~520 tests across
`gsp-config`, `gsp-core` (unit + `crates/gsp-core/tests/{tcp_forward,udp_forward,amplification}.rs`),
`gsp` (incl. the `sniffer_loader` WAT-fixture end-to-end and the admin HTTP tests),
`gsp-controller` / `gsp-aggregator` / `gsp-ui` / `gsp-agent` (in-module HTTP and unit
tests) and `crates/gsp-fleet-tests`' real-multi-process tests (`fleet`, `gossip`,
`tunnel_addresses`; they debug-build and spawn the real binaries, adding real wall-clock
time). Needs `protoc` on `PATH`. Not part of `make check`:

- the namespace lab `make tunnel-e2e` / `make tunnel-e2e-ci` (12 scenarios, see "Known
  flakes & environment gotchas");
- `make ui-test` (vitest), `make deploy-lint` (daemon-free render checks of `deploy/`;
  needs the docker CLI + ruby), `make deploy-images` / `make deploy-smoke` / `make deploy-scan` (need a
  Docker daemon; the scan also needs `trivy`);
- the CI helper scripts: `sh .github/scripts/changes_test.sh` and
  `python3 .github/scripts/test_summary_test.py` and `python3 .github/scripts/trivy_summary_test.py` (CI runs all three in the `changes` job).

CI runs Rust tests under `cargo nextest` (each test in its own process) — see
"Infra / environment".

- `TP­ROXY` / `IP_TRANSPARENT` e2e is not in CI (needs `CAP_NET_ADMIN`) — covered
  by config parse/reject + `connect_tcp_from` fallback tests. Setup recipe in
  `docs/04`.
- The UDP `prefix:` e2e needs a Linux host with `IP_PKTINFO` and loopback
  `127.0.0.2` / `127.0.0.3`; not portable to macOS/Windows CI.
- `#[ignore]`d, run in the `plugins` CI job after `make plugins`: the first-party
  `.wasm` artifact round-trip and the WASM-boundary N1 latency bench.
- `wasmtime` is a normal `cargo` dep — it does **not** need the
  `wasm32-unknown-unknown` rustc target; that target is only needed to *build*
  the plugin crates. Loader tests assemble WASM from inline WAT via the `wat`
  crate.

---

## Infra / environment

- Toolchain via `rustup` (`stable`). If `cargo` isn't found:
  `export PATH="$HOME/.cargo/bin:$PATH"`.
- **`protoc` is a build requirement** (gRPC resolver codegen in
  `crates/gsp/build.rs`). CI installs `protobuf-compiler`.
- CI: `.github/workflows/ci.yml`. Docs-only pushes (`**.md`, `docs/**`, `LICENSE-*`) don't run it,
  and a newer push cancels an older run. A `changes` job (`.github/scripts/changes.sh`, tested by
  `changes_test.sh`, self-run in CI) decides which path-scoped jobs run: `ui`, `plugins`, `tunnel`,
  `deploy`, `fuzz`; `test` and `audit` always run for non-docs pushes, and `trivy` follows
  `deploy` (it scans `deploy`'s images, handed over as the 1-day `deploy-images` artifact
  of `docker save` tarballs). `trivy` and `audit` are informational: `continue-on-error`,
  results on the run's summary page, as warning annotations and as artifacts
  (`trivy-reports`, `cargo-audit`). A workflow edit, a failed diff, the
  **nightly schedule (03:17 UTC)** and `workflow_dispatch` run everything — so `deploy` and `fuzz`
  also act as nightly canaries for code-driven breakage. Adding a job or moving files between
  areas means updating `changes.sh` *and* its test. A full run bills roughly 60–80
  runner-minutes (each job rounds up to the minute; cold caches cost more); the scan
  jobs add under a minute each.
  Cargo caching is `.github/actions/cargo-cache` (rolling: a new snapshot per `main` push,
  restored by prefix `os-rustc-Cargo.lock`; PRs read it but don't write). It replaced
  `Swatinem/rust-cache`, which only saves on an exact-key miss — its key is the lockfile
  hash, so its snapshot froze at the last `Cargo.lock` change and a release-profile edit
  (`strip = true`) left `plugins` recompiling all 342 crates every run. `fuzz` still uses
  `rust-cache` (nightly only, tiny). A lockfile or toolchain bump starts cold by design.
  Verified: a cold run took test 8m41 / build-release 10m53 / deploy 11m25; the next run
  restored the previous commit's snapshot (build-release recompiled 7 crates, 4m02, the rest
  is the thin-LTO link) and test took 3m53, deploy 1m22. Re-measured 2026-10-02 after the
  lockfile bump for `gsp-http`: cold `test` 8m15 / `build-release` 16.5 min, then warm
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
  `build-release` also runs `cargo test -p gsp --release --no-run`: cargo unifies features per
  invocation, so the plugins job's `-p gsp` test build needs different dependency artifacts
  than the five-binary build, and it recompiled 293 crates even on an exact cache hit until
  the snapshot held both.
  **Shared release stage:** a `build-release` job compiles the five release binaries once
  (cache namespace `release`); `plugins` restores that snapshot (same key => exact hit) and
  `deploy` downloads the binaries as an artifact and builds the images with
  `BIN_SOURCE=prebuilt` (`deploy/Dockerfile`). Nightly `deploy` uses the self-contained
  in-Docker build instead, so that path can't rot. These three jobs are pinned to
  `ubuntu-24.04` (glibc 2.39): `ubuntu-latest` becomes Ubuntu 26 on 2026-10-19 and binaries
  built there might need a newer glibc than the distroless runtime's 2.41. (Since
  2026-10-02 every other job is pinned to `ubuntu-24.04` too.) `changes.sh`
  emits a `release` flag (= plugins or deploy). Debug jobs (`test`, `tunnel`) deliberately
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
- git remote `github.com/Wueschli/gameserver-proxy`, branch `main`; `git push`
  works, `origin/main` is current. The HTTPS credential helper logs a harmless
  "nonexistent Windows path" warning before falling back to a working credential.
- `Cargo.toml` declares `MIT OR Apache-2.0` with `LICENSE-MIT` / `LICENSE-APACHE`
  included. No explicit user decision on record — confirm if it matters.

---

## Open questions carried from `docs/01-requirements.md`

- Does one client ever need **two backends at once** (TCP control + UDP gameplay
  on different instances)? Affects the session model.
- Is **QUIC-aware routing** (connection ID) needed, or is opaque UDP enough?
  Assumed opaque.
- Cross-instance session failover: assumed **no** for v1 (ADR 4).
