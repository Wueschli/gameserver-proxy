# Phase 14 tunnel end-to-end test (rootless, network namespaces)

Status: **design — awaiting review.** Part 1 of the post-CI-stabilization
"phase 14 hardening" work; parts 2–4 (deployment packaging, controller-behind-TLS
docs, tunnel address authority) get their own specs.

## Understanding (what was asked, what I assumed)

Said by the user: phase 14 (WireGuard backend transport) is the newest and
least-covered code; its only live verification was a one-off Docker harness that
lives in a session scratchpad, so CI covers none of it. The agreed order is
(1) this e2e test, (2) deployment packaging, (3) controller-behind-TLS docs,
(4) tunnel address authority.

Assumed (correct me): the goal is a regression net that runs in CI and on a dev
box **without Docker and without root**, exercising the *same release binaries
and flags* a deployment uses. It is not a replacement for container packaging
(part 2) and does not test container capabilities (`docs/12` already documents
those; slice 6 verified them once).

Success criteria:
1. `make tunnel-e2e` passes on this machine as an unprivileged user, and in CI.
2. A real payload crosses client → `gsp` → WireGuard → `gsp-agent` → backend → back,
   over TCP and UDP.
3. The slice-6 regression (session torn down on every unchanged re-registration)
   would be caught.
4. Slice 7 (a proxy added *after* the origin is up is learned without restarting
   the agent) is verified live for the first time.
5. `make check` and plain `cargo test` are unaffected (the test is `#[ignore]`).

Non-goals: Docker/K8s packaging, CGNAT/relay, address allocation, load/throughput.

## Why network namespaces, not Docker

Verified on this machine: `unshare -Urnm` works unprivileged (`veth`, TUN and
the kernel `wireguard` module all usable inside it; `/dev/net/tun` is mode 666).
The Docker daemon is not running here, and a Docker-based test needs image builds
in CI. Namespaces are faster, hermetic, and the whole lab dies with the parent.

## Topology

One outer lab (`unshare -Urnm --kill-child`) plus two child netns created with
`unshare -n sleep infinity` and addressed by PID (`nsenter --net=/proc/PID/ns/net`;
`ip netns` is avoided because it needs a writable `/var/run/netns`).

```
lab ns (outer)             edge ns                 origin ns
 gsp-controller             gsp --tunnel-*          gsp-agent
 client (test process)      wg iface 10.60.0.1      wg iface 10.60.0.2
 ip_forward=1  ── veth ──►  10.99.1.2/30            echo backend 10.60.0.2:7000 (tcp+udp)
   10.99.1.1/30  ── veth ──────────────────────────► 10.99.2.2/30
   10.99.2.1/30
(edge2 ns, scenario 3 only: 10.99.3.2/30, wg 10.60.0.3)
```

The lab ns forwards between edge and origin: it is the "internet". WireGuard UDP
rides that underlay; the tunnel addresses (`10.60.0.0/24`) exist only on the WG
interfaces. The client runs in the lab ns and dials `gsp`'s listener at
`10.99.1.2:8000`.

## Components

- `crates/gsp-fleet-tests/src/netns.rs` — `Lab` helper: create child ns (holder
  `sleep` process, killed on drop), veth pairs, addresses/routes, `ip_forward`,
  and `spawn_in(ns, bin, args)` returning the existing `Proc`. Shells out to
  `ip`/`nsenter` (present on every runner image; no new Rust deps).
- `crates/gsp-fleet-tests/src/lib.rs` — `build_fleet_bins` also builds
  `gsp-agent`; a small TCP + UDP echo-server helper that runs on a dedicated
  thread which first enters the origin ns via `nix::sched::setns` (safe wrapper;
  netns membership is per-thread, so the rest of the test stays in the lab) and
  then runs a current-thread tokio runtime. No extra binary, no `unsafe`
  (`nix` is already a workspace dependency; this crate gains its `sched`
  feature).
- `crates/gsp-fleet-tests/tests/tunnel.rs` — the scenarios, all `#[ignore]`.
  On start it checks it is inside a lab with `CAP_NET_ADMIN` and fails loudly
  ("run via `make tunnel-e2e`") rather than skipping silently — it only runs when
  explicitly requested.
- `Makefile`: `tunnel-e2e` = `unshare -Urnm --kill-child cargo test -p
  gsp-fleet-tests --test tunnel -- --ignored --test-threads=1` (runs directly if
  already root). The namespace also gets a tmpfs on `/run` with `/run/wireguard`
  (boringtun's control socket lives there). `TUNNEL_BACKEND=kernel|userspace`
  selects the WG backend; default `kernel`.
  **Spike findings (2026-10-01, real binaries in `unshare -Urnm`):** both backends
  work. Kernel: first round trip in ~2 s. Userspace (`boringtun`): the first
  round trip took ~25 s — the proxy has no endpoint for the origin, so the
  handshake waits for the agent's 25 s persistent keepalive. Per-wait deadlines
  are therefore backend-scaled (30 s kernel / 90 s userspace), and CI runs both
  as a matrix. The slow userspace first handshake is a product observation, not
  fixed here (recorded in `HANDOVER.md`).
- CI: a `tunnel` job. If the runner restricts unprivileged userns (Ubuntu 24.04
  AppArmor), set `kernel.apparmor_restrict_unprivileged_userns=0` via `sudo
  sysctl`, or run the make target under `sudo`. Starts non-blocking
  (`continue-on-error`) until it has been green for a while, then becomes
  required.

## Bring-up order (forced by the schema)

`backend_sources[].pubkey` pins the origin's key in `gsp`'s config, so the key
must exist before `gsp` starts:
1. `gsp-controller` (lab). 2. echo backends + `gsp-agent` (origin; stable key
persisted in its data dir). 3. Poll the controller's `GET /peers/<name>` until the
origin appears; read its pubkey. 4. Write `gsp`'s config (tunnel source +
pool + TCP/UDP listener) with that pubkey and start `gsp --tunnel-*` (edge).
5. Wait for a **real round trip**, not for `/pools` to say healthy: a new backend
   starts optimistically healthy, so `/pools` is true before the tunnel is up.

## Scenarios

1. **Round trip.** The origin's echo server starts *after* `gsp`, so the backend is
   first seen unhealthy and must turn healthy on its own. Then a 256 KiB TCP
   payload and a 1200-byte UDP datagram come back byte-for-byte (large enough to
   cross the WireGuard MTU). Proves handshake, routing, `AllowedIPs`, discovery
   and health end to end.
2. **Re-registration stability** (slice-6 regression). Agent and proxy register
   every 1 s. Probe every 200 ms for ~10 s after the first success; require 100 %
   success. A teardown/rebuild per registration (the original bug) breaks the
   handshake and fails this. No dependency on the `wg` CLI.
3. **Late proxy** (slice 7). With the origin up and the agent never restarted,
   start `edge2` (new ns, new tunnel address and keypair); wait for the agent to
   peer it from the proxy-peers registry; round-trip through `edge2`.
4. **Pinned key mismatch refused.** A `gsp` whose `backend_sources[].pubkey` does
   not match the registered one never gets a backend: pool stays empty / client
   connection fails, and traffic does not flow. Guards the "key change refused
   loudly" property.

## Failure handling / flake control

Every wait is poll-until-true with a deadline (reuse `wait_until` /
`wait_http_up`); no fixed sleeps before connects (the lesson of the 2026-10-01
flake fix). Ports in the lab are fixed (private ns, no collisions), so
`free_port` is not needed. All children are `kill_on_drop`; `--kill-child` reaps
anything a panic leaves behind. On failure the test prints each child's
captured stderr (stderr is piped to a temp file per process, unlike the
existing helpers which discard it).

## Risks

- Hosts where unprivileged userns is disabled/restricted → documented in the
  `make` target's error message and the CI `sysctl`/`sudo` fallback.
- The kernel backend needs the host's `wireguard` module (CI runs
  `modprobe wireguard`); the userspace run is the portable fallback.
- Debug builds plus WG userspace are slower; deadlines are backend-scaled (see
  above) and the job is non-blocking initially.

## Docs to update when built

`docs/08` phase 14 (slice 7 now live-verified; CI coverage exists), `HANDOVER.md`
(drop "Docker harness lives only in a scratchpad" follow-up; describe
`make tunnel-e2e`), `AGENTS.md` commands table and `gsp-fleet-tests` layout line,
`docs/12` (pointer: the namespace test verifies logic, container caps stay
documented there).
