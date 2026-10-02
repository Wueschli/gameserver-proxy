# HANDOVER

Current state and the traps. This file is the *current-state + gotchas* layer only.
Design is the source of truth in [`docs/`](docs/); locked decisions are the ADR table
in [`docs/09-technology-choices.md`](docs/09-technology-choices.md). Per-slice
implementation history lives in `git log` and [`docs/08-roadmap.md`](docs/08-roadmap.md),
not here.

Last updated: 2026-10-02 (custom-CA `--ca-file` session; see "Resume here").

## Current state

**All roadmap phases (0–14) are built, individually verified live, and covered by
`make check`.** No slice is in flight — the repo is at a natural stopping point.
Remaining work: the "Known follow-ups" table below, and making the `tunnel` and
`deploy` CI jobs blocking once stable.

### Resume here (written for picking this up on another machine)

State at 2026-10-02 (later): the `--ca-file` work is on branch
`claude/dazzling-carson-j2yxj0` (PR to `main`). It changes `Cargo.lock` (new `gsp-http`
crate), so its CI run starts cold again and does **not** confirm warm-cache timings — the
first code push after it does. Earlier: CI run
36989065039 was green on every job it ran. That run was **cold** (the lockfile changed,
and the rolling cache starts cold on a lockfile bump by design): `build-release` took
17 min and `test` 9m41. Expect a normal code push to be much faster (~4 min each, see
"Infra / environment") — **verify that on the next code push**; if it is still slow, the
cache key in `.github/actions/cargo-cache` is the first suspect.

Open decisions for the owner:

1. **Make the `tunnel` and `deploy` CI jobs blocking** (drop `continue-on-error: true` in
   `.github/workflows/ci.yml`). Both are green on the latest runs (`tunnel` on both
   backends); `deploy` had exactly one real failure, a lint bug when its prebuilt-binaries
   mode first ran (fixed). Cost to know first: each `tunnel` leg now takes ~9.5 min in CI,
   mostly the kernel edge-restart scenario (see the follow-up on that outage).
2. **Native TLS for `gsp-controller` is wanted eventually** (owner, 2026-10-02). It bundles
   serving TLS itself plus TLS for HA/adopt traffic (follow-up below); custom CA
   support (`--ca-file`) landed 2026-10-02. Until then docs/12 "gsp-controller behind
   TLS" is the supported pattern. New controller or client code must not hard-code
   `http://`, and must build HTTP clients via `gsp_http::{client, builder}` so
   `--ca-file` applies.
3. **Publish the reference images?** The owner chose "reference only" (2026-10-01).
   Publishing (GHCR on release tags, multi-arch if arm64 is needed) is a small follow-up:
   `deploy/Dockerfile` already has the `BIN_SOURCE` switch; it needs a release workflow,
   tags and a registry login.

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
  the VPS?). Only relevant if self-hosting is tried again.

Suggested order: (a) confirm the warm-cache CI timings on the next code push, (b) decide
on blocking CI jobs, (c) pick from "Known follow-ups" — the owner's stated interest is
native TLS and the deferred pieces of the address authority.

Most recent landings (newest first; full history in `git log`):

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
  `deploy` (non-blocking until stable). **First CI run (36922142697, 2026-10-01) was
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

- **`make audit`** wraps `cargo audit` (needs `cargo install cargo-audit --locked`).
  New advisories land on a schedule you don't control — on 2026-10-01 it caught
  `rustls` (RUSTSEC-2026-0285) and two `wasmtime` fuel-accounting advisories
  (RUSTSEC-2026-0315/0316), fixed by lockfile-only patch bumps (`rustls` 0.23.45,
  `wasmtime` 48.0.3). Three "unmaintained crate" warnings (`atomic-polyfill`,
  `fxhash`, `instant`) are informational and don't fail the target. Not in CI
  yet — run it before a release.
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
  The CI job is **non-blocking** (`continue-on-error: true`); making it required is an
  open decision (see "Resume here"). It is green on both backends on GitHub (`unshare -Urnm`
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
| Tunnel address authority — deferred pieces (decided out of scope 2026-10-02, owner wants them later) | Spec: `docs/superpowers/specs/2026-10-02-tunnel-address-authority-design.md`: **IPv6** tunnel networks; **HA-replicated allocation** (the registries aren't Raft-integrated, so `--tunnel-network` + `--ha-peers` is refused at startup); **automatic lease expiry** (v1 is explicit release + a stale warning); a **gsp-ui view** of `GET /tunnel/addresses`; **changing a live peer's address without a restart** (v1 logs the mismatch and keeps running); `TunnelSource` **dropping pool entries when an origin is deleted** (a `404` still means "keep last-known-good") |
| Address authority — deferred review minors | `warn_stale` is silent on a storage error; `allocate()` is an O(allocated) scan under the global mutex and `allocated()`/`Exhausted` use `Tree::len()` (O(n)) — consider capping `--tunnel-network` size; `check_pin` treats an unparseable holder as free; `parse_duration` can overflow (use `checked_mul`); stored addresses are not re-validated if `--tunnel-network` later changes; a store failure after a successful claim also keeps the claim (the doc comment only mentions the backend-422 case), and a stream of distinct names with bad backends can use up the pool (bearer-gated; DELETE + the stale warning are the remedy); every 4xx is treated as a permanent registration failure incl. 408/429 (consider transient); a changed `--address` pin loses to the saved address on a transient failure without notice; the final transient error is not logged when falling back to the saved address; a name re-registered with a NEW pubkey never removes the old key's peer (pre-existing); `the_production_client_has_a_request_timeout` waits ~10 s; lab: scenario 8 does not assert the edge came up ON its saved address, `agent_refused` loses the agent log on timeout, `start_controller` drops failed attempts' logs and its sled-lock comment may be wrong, scenario 7 asserts stickiness only after the 200 s wait, `restart_edge` has a redundant sleep and deletes the shared boringtun socket path (safe only for single-edge scenarios) |
| Kernel WireGuard: a restarted edge `gsp` leaves the tunnel down for ~2.5 min (found 2026-10-02; pre-dates the address work) | The edge has no endpoint for the origin so it cannot start a handshake; the agent sees an identical proxy registration so never re-sets the peer; keepalives do not re-key a session it still believes valid; recovery waits for WireGuard's 120 s rekey. A possible fix is a boot id in the proxy registration (protocol change), not done. The lab's restart scenario therefore waits up to 200 s after an edge restart (`wait_roundtrip_after_restart`), which adds ~2.5 min to the kernel `tunnel` CI leg. |
| Native TLS in `gsp-controller` (owner wants it eventually, 2026-10-02) | Umbrella: terminate TLS in the controller itself (not designed), plus the TLS-for-HA/adopt row below. Clients trusting a custom CA is done (`--ca-file`, 2026-10-02). Today's supported pattern is a reverse proxy (docs/12 "gsp-controller behind TLS"). Keep base URLs/schemes configurable in any new code so this stays small. |
| Publish the reference images | Reference-only today (owner's choice). GHCR on release tags (+ multi-arch if arm64 is needed): a release workflow, tags and a registry login; `deploy/Dockerfile`'s `BIN_SOURCE` switch already supports building from CI-built binaries. |
| TLS for HA / adopt traffic | `/raft/*`, forwarded writes and `/admin/adopt` hard-code `http://` (`ha/network.rs:45`, `ha/client.rs:83`, `adopt.rs:220`); today they must stay on a private network, protected only by `--ha-token` |
| Terse reqwest errors in fleet clients | Most clients format `reqwest` errors with `{e}`, whose `Display` drops the cause chain, so a TLS/CA failure reads only "error sending request". Fixed for `gsp`'s `controller_client` (initial fetch + subscribe connect) with the `--ca-file` work; the rest (`intent_client`, `tunnel_client`, `aggregator_client`, `gsp-agent`, `gsp-controller` parent/relay, `gsp-ui` proxies) still do it — use `anyhow` context / `{e:#}` |
| `--ca-file` — deferred review minors (2026-10-02) | docs/12 "Pointing the clients at it" has a line starting with `: ` after the re-wrap (renders as "base URL : …"); `CaError::{Read,Parse}` print `{source}` and also expose it as the source, so anyhow shows the cause twice and `Parse`'s top line ends in reqwest's bare "builder error"; `controller_client`'s reconnect warning records `error = format!(..)` (a quoted `String`) where `%format_args!(..)` matches the file's style; no test sets `--ca-file` against a plain `http://` endpoint (correct by construction); `crates/gsp-http/tests/fixtures/leaf.key` may need a secret-scanner allowlist entry if one is ever enabled |
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
  needs the docker CLI + ruby), `make deploy-images` / `make deploy-smoke` (need a Docker
  daemon);
- the CI helper scripts: `sh .github/scripts/changes_test.sh` and
  `python3 .github/scripts/test_summary_test.py` (CI runs both in the `changes` job).

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
  `deploy`, `fuzz` (`test` always runs for non-docs pushes). A workflow edit, a failed diff, the
  **nightly schedule (03:17 UTC)** and `workflow_dispatch` run everything — so `deploy` and `fuzz`
  also act as nightly canaries for code-driven breakage. Adding a job or moving files between
  areas means updating `changes.sh` *and* its test. A full run is ~32 runner-minutes.
  Cargo caching is `.github/actions/cargo-cache` (rolling: a new snapshot per `main` push,
  restored by prefix `os-rustc-Cargo.lock`; PRs read it but don't write). It replaced
  `Swatinem/rust-cache`, which only saves on an exact-key miss — its key is the lockfile
  hash, so its snapshot froze at the last `Cargo.lock` change and a release-profile edit
  (`strip = true`) left `plugins` recompiling all 342 crates every run. `fuzz` still uses
  `rust-cache` (nightly only, tiny). A lockfile or toolchain bump starts cold by design.
  Verified: a cold run took test 8m41 / build-release 10m53 / deploy 11m25; the next run
  restored the previous commit's snapshot (build-release recompiled 7 crates, 4m02, the rest
  is the thin-LTO link) and test took 3m53, deploy 1m22.
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
