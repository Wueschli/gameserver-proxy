# 11 – Backend transport

**Status: built (2026-09-06).** All 7 Phase 14 slices are implemented and
verified live end-to-end in Docker (see `docs/08-roadmap.md` "Phase 14" for the
slice-by-slice record and `HANDOVER.md` for the bugs the live run surfaced).
This chapter remains the design source of truth for the mechanism. The design
was locked in a 2026-09-05 session at the depth of the phase-12/13 pre-build
sessions; the "Locked decisions" and "Open questions" sections below are kept
as written then.

## Problem

Every requirement and architecture decision so far assumes what
[`docs/01-requirements.md`](01-requirements.md) states outright under
Assumptions: *"Backends are reachable over a trusted internal network."*
That's true for a single-region deployment (proxy and game servers in the
same DC/VPC). It stops being true the moment the fleet is what phase 10–13
were actually built for: **proxy instances distributed globally** (edge
PoPs, one per region) in front of game servers that are **not** on those
proxies' local network — a different DC, a different cloud, a rented box at
a small hosting provider, sometimes behind a home NAT.

Today `connect_backend` (`crates/gsp-core/src/proxy.rs:138`) and
`connect_upstream` (`crates/gsp-core/src/listener_udp.rs:844`) do a plain
`TcpStream::connect`/`UdpSocket::connect` to a bare `SocketAddr` — there is no
seam between "route resolved to this address" and "dial it" for anything
else to hook into. `health.rs`'s active checks dial the same way
independently. Closing this gap is Phase 14.

## Don't reinvent the wheel

Before designing anything new, what does this problem already have solved
prior art for?

- **Steam Datagram Relay** (Valve) is the closest large-scale precedent: game
  clients never see the origin server's IP; traffic is relayed through
  Valve's own network, and origin servers register only with the relay
  fabric. Proprietary infrastructure, not reusable, but it validates the
  *shape* of the answer: a relay/tunnel network in front of an origin that
  never needs a public inbound port of its own.
- **rathole** / **frp**: exactly this problem in miniature (a lightweight,
  high-performance reverse-tunnel proxy for NAT traversal, written in Rust —
  `rathole` specifically). Real, working, maintained tools — but they're
  standalone binaries with their own framing/auth protocol, not something to
  embed as a library, and they know nothing about pools, health checks, or
  this project's control plane. Good validation that "origin dials out to a
  relay with a public IP" is the standard shape; not something to depend on
  directly.
- **WireGuard** itself, unmodified, is the actual answer for the tunnel
  data plane. It already solves the hard, security-critical part (an
  authenticated, encrypted point-to-point link) as a small, audited,
  extremely widely deployed protocol — exactly the "defer to a maintained,
  audited implementation for a subtle, security-relevant mechanism"
  reasoning ADR 21 (Raft) and ADR 24 (SWIM gossip) already established
  twice in this codebase. Critically, **WireGuard already works when only
  one side has a public, reachable endpoint**: the origin-side peer needs no
  public IP or open inbound port at all — it just needs to be able to send
  outbound UDP to the proxy's known public endpoint once, and WireGuard's
  own roaming (the far side updates its record of "where that peer's
  packets are coming from" on every received, authenticated packet) plus a
  keepalive keeps a NAT mapping open indefinitely. This is true even for an
  origin behind a home router — no port forwarding, no static IP, ever
  required on the origin side. (The one case this doesn't cover — both
  sides behind restrictive/CGNAT-style NAT with no cooperating public peer
  at all — doesn't apply here, since the proxy side is edge infrastructure
  and always has a real public IP.)
- **Headscale** (a BSD-3 self-hosted Tailscale-protocol coordination
  server) is the closest prior art for *automated WireGuard key/peer
  distribution at fleet scale* — proof that "a control plane that hands out
  WireGuard peer configs" is a well-trodden, sensibly-scoped idea. Adopting
  it wholesale would mean either running real Tailscale clients (a large,
  general-purpose product with its own protocol, unrelated to pools/health/
  routing) or reimplementing enough of its coordination protocol to be
  compatible — neither is smaller than extending the control plane this
  project already runs (`gsp-controller`) with one narrow registry. Its
  *idea* is reused; the software is not.

**Conclusion**: nothing about the tunnel itself needs inventing. Real,
unmodified WireGuard (kernel module where available, `boringtun`'s
userspace implementation as a portable fallback) is the entire data plane.
The only genuinely new code this project needs is (a) a small origin-side
helper that manages a local WireGuard interface and registers itself, and
(b) a narrow extension to the existing Tier-1 control plane
(`gsp-controller`) that distributes peer configuration automatically instead
of by hand — both thin, both specific to fitting this into pools/health/
routing, neither a reimplementation of cryptography or tunnel framing.

## Shape

```
┌─────────────┐  WireGuard  ┌──────────────┐        ┌──────────────┐
│  gsp (edge, │◄───────────►│  gsp-agent   │◄──────►│  game server │
│  public IP) │   tunnel    │  (origin,    │ plain   │  process     │
│             │             │  no public   │ local   │              │
│  pool target│             │  IP needed)  │ traffic │              │
│  = tunnel-  │             └──────────────┘        └──────────────┘
│  internal IP│
└─────────────┘
        ▲
        │ peer config (pubkeys, allowed IPs)
        ▼
┌───────────────────────┐
│  gsp-controller        │  (existing Tier-1 store, ADR 20/21) —
│  + new "backend peers" │  new small registry, distributed the
│  registry              │  same way config revisions already are
└───────────────────────┘
```

- **The data plane never changes.** Once a WireGuard interface for a given
  origin exists on the proxy host, that origin's backends are just IP
  addresses on that interface's subnet — `connect_backend`/
  `connect_upstream`/`health.rs`'s dials need **zero code changes**, because
  from `gsp`'s point of view a tunneled backend looks exactly like any other
  routable `SocketAddr`. This is the same reasoning that already lets this
  project's transparent-proxy mode (ADR 12) and every other listener option
  layer onto the existing pump without touching it.
- **`gsp-agent`** (new, small binary, its own workspace crate — locked,
  matching the `gsp-controller`/`gsp-aggregator`/`gsp-ui` precedent of one
  small process per concern, keeping WireGuard dependencies out of the
  proxy-critical `gsp` binary entirely). Runs next to the actual game server
  process. Creates and maintains **one shared local WireGuard interface**
  with every proxy PoP it's paired with as a peer on that same interface
  (locked — the standard WireGuard shape: one interface, many peers, each
  scoped by its own `AllowedIPs`; per-origin interfaces would multiply
  routing-table entries for no isolation benefit `AllowedIPs` doesn't
  already give per peer). Registers its public key + which backend
  address(es) it fronts with `gsp-controller`. It does not proxy or inspect
  game traffic itself — the WireGuard interface does that, and the real
  game server binds normally on its own loopback/interface address, exactly
  as it would with zero knowledge that a proxy is involved.
- **Interface management via `defguard/wireguard-rs`** (locked) — a
  multi-platform Rust library unifying kernel-netlink and `boringtun`-
  userspace configuration behind one API, used by both `gsp-agent` (the
  origin's interface) and `gsp`'s new reconcile task (the proxy's shared
  interface, all origins as peers). One abstraction instead of hand-rolling
  the kernel-vs-userspace branch ourselves on top of two separately-focused
  crates (`wireguard-uapi` + `boringtun`) — continues this project's own
  "prefer a maintained wrapper over hand-rolled plumbing" bias (`nix` for
  `recvmmsg`/TPROXY/`splice`, ADR 10/12).
- **`gsp-controller` gains a "backend peers" registry** — a new resource
  alongside the existing config-revision log and phase-12 intent log, not a
  replacement for either. `gsp-agent` writes its own registration (pubkey,
  allowed backend addresses, last-known endpoint); every edge `gsp` instance
  subscribes to the resulting peer table the same way it already subscribes
  to config (`controller_client.rs`'s existing shape, ADR 13) and reconciles
  its shared WireGuard interface's peer list to match (new `gsp` task, same
  shape as `controller_client::run`, using `wireguard-rs` above). Revoking/
  rotating an origin's key is a controller-side removal that propagates the
  same way a config change does — no manual per-proxy key exchange, ever,
  for a fleet of N proxies × M origins.
- **...and a mirror-image "proxy peers" registry, built in slice 7.** The
  bullet above only solves proxy→origin discovery. `gsp-agent`'s "every
  proxy PoP it's paired with as a peer" premise two bullets up needs the
  other direction too: an origin has to learn about every proxy, including
  ones added after it was deployed, without being restarted. So every
  `gsp --tunnel-*` instance also registers itself (pubkey + its own public
  endpoint — always known, unlike an origin's) with a second registry, and
  every `gsp-agent` subscribes to it and reconciles proxies onto its own
  interface, the exact mirror of the first bullet's mechanism run in the
  other direction. `gsp-agent --peer-pubkey`/`--peer-endpoint` (a manual
  static pin, what slice 6's first end-to-end verification used before
  this existed) still work alongside it for a bootstrap proxy or a
  deployment too small to bother with the registry — they converge to the
  same interface state a registered proxy would reach anyway, so there's no
  conflict between the two paths.
- **Edge restarts: a per-process `boot_id` (built 2026-10-03).** A restarted
  `gsp --tunnel-*` has a fresh interface but no endpoint for any origin
  (origins may sit behind NAT), so only the origin can re-handshake — and with
  kernel WireGuard the origin's session to the old process still looks valid,
  so it used to wait for the 120 s rekey (~2.5 min outage). Every proxy
  registration therefore carries a random `boot_id` (128-bit hex, new per
  process start; the controller accepts 1–64 of `[A-Za-z0-9-]` and stores it
  as-is). `gsp-agent` compares whole registrations, so a changed `boot_id`
  re-sets the peer (remove + add), which drops the dead session; the re-added
  peer's persistent keepalive starts a new handshake at once. Optional on the
  wire in both directions: an older proxy sends none (no change from before),
  an older controller drops it, an older agent ignores it.
- **Key material stays out of the config-revision log.** A peer table is
  security-sensitive, high-churn (origins come and go), and has nothing to
  do with routing/pool structure — it belongs in its own resource on the
  same Tier-1 store, mirroring how the intent log (phase 12) already lives
  alongside the config log rather than inside it.
- **Config schema: an origin is a new `BackendSource` (locked)** — reuses
  the existing level-triggered discovery seam (ADR 12a: `fetch() -> Vec
  <SocketAddr>`, the runtime diffs it, same reconcile path DNS-SRV/Consul/
  Kubernetes sources already use) instead of inventing a parallel "origins:
  / pools[].origin" concept. A pool referencing an origin looks exactly
  like a pool referencing any other dynamic source today:
  `pools: [{ name: p, source: my-origin }]`, `backend_sources: [{ name:
  my-origin, type: tunnel, ... }]`. The "tunnel" source's `fetch()` returns
  the origin's currently-registered, tunnel-internal backend address(es)
  from the backend-peers registry — no new schema *concept*, one new
  `BackendSource` implementation and one new `backend_sources[].type`
  variant. This also means an origin's backend address(es) can change
  (the agent re-registers) without touching `gsp-config` at all, the same
  "discovered set, not a static file list" property every other dynamic
  source already has.

## Address authority (built 2026-10-02)

`gsp-controller` allocates tunnel-internal addresses, so no operator chooses (or
mis-chooses) one. Design and decisions:
[`docs/superpowers/specs/2026-10-02-tunnel-address-authority-design.md`](superpowers/specs/2026-10-02-tunnel-address-authority-design.md);
IPv6 (built 2026-10-03):
[`docs/superpowers/specs/2026-10-03-ipv6-tunnel-design.md`](superpowers/specs/2026-10-03-ipv6-tunnel-design.md).

- **Allocation.** Start the controller with `--tunnel-network fd49:89c1:4b5e:60::/64`
  (IPv6, `/64` to `/120`; the default in the docs and examples, and a ULA cannot clash
  with an origin's own LAN) or `--tunnel-network 10.60.0.0/16` (IPv4, `/16` to `/30`).
  One family per controller and one address per peer; choose IPv4 when a game server
  binds `0.0.0.0` only, since it cannot be reached on an IPv6 tunnel address. Either
  family holds at most 65 534 entries (pins included), because allocation scans the
  pool; `capacity` in `GET /tunnel/addresses` is the smaller of that and the host
  count. The network and all-ones addresses are never handed out, and IPv4 written as
  IPv6 (`::ffff:a.b.c.d`) is refused. A registration that omits its address is allocated the lowest
  free host address; the allocation is sticky per `(role, name)`. A registration may
  instead **pin** an address, which is granted only if free (`409` otherwise). One
  global address space is shared by origins and proxies. Without `--tunnel-network`
  the controller is pin-only: it enforces uniqueness but allocates nothing, and accepts
  pins of either family (mixing them is the operator's responsibility: peers of
  different families cannot reach each other).
- **Underlay.** Independent of the tunnel's family: WireGuard endpoints
  (`--endpoint`, `--tunnel-endpoint`, `--peer-endpoint`, written `[2001:db8::7]:51820`
  for IPv6) and the controller URL (`http://[fd99::1]:7070`) may be IPv4 or IPv6.
- **Changing the network.** At startup the controller refuses to run when stored
  addresses fall outside `--tunnel-network`, naming up to ten of them. Restore the
  previous network, or pass `--tunnel-readdress`: the controller then starts, warns
  once, and gives each such peer a new address at its next registration. The running
  peer logs the change and keeps its old address until it restarts, so its traffic is
  interrupted until then. Peers that are offline keep their old entry until they
  register again; `DELETE` the ones that will not come back. `--tunnel-readdress` is
  refused without a network and together with `--ha-peers` or `--ha-join`, and is
  harmless when nothing is outside the network.
- **Startup.** `gsp-agent` and `gsp --tunnel-*` register *before* bringing their
  interface up (the answer is its address), persist the answer next to their key, and
  can start from it while the controller is down. For `gsp --tunnel-*` that fallback
  is not instant: with the controller down it first spends its ~30 s registration
  budget, then falls back to the saved address (`<tunnel-key-file>.address`), logging
  the last error and, if a pinned address differs from the saved one, that the pin
  needs a restart once the controller is back. `408`/`429` count as "controller unavailable", any other
  `4xx` refuses startup. A later,
  different answer is logged as an error and not applied until a restart.
- **Routing.** Every peer is a host route (`/32`, or `/128` for IPv6): origins route each proxy's tunnel address
  (this fixed the earlier `AllowedIPs 0.0.0.0/0` bug where a second proxy stole the
  first one's route), proxies route each origin's. Backends must be on the
  registrant's own address; `--backends :25565` means "my address, port 25565" (stored
  as `[fd49:89c1:4b5e:60::1]:25565` on an IPv6 network).
- **Release.** `DELETE /peers/{name}` / `DELETE /proxy-peers/{name}` free the address
  and emit a tombstone that subscribers turn into a WireGuard peer removal.
  `GET /tunnel/addresses` lists the table with a `stale` flag
  (`--tunnel-stale-after`, default 14 days); nothing is freed automatically.
  `gsp-ui` shows the same table, read-only, on its Tunnel addresses page
  (`GET /api/tunnel/addresses`, proxied with `--controller-url`/`--controller-token`).
- **High availability (built 2026-10-03).** `--tunnel-network` works with `--ha-peers`:
  both registries and the address book are replicated through Raft, so every node
  serves the same registrations and addresses, and a registration or `DELETE` made on
  any node (a follower forwards it to the leader) is allocated once, cluster-wide.
  The leader records its `--tunnel-network` when the cluster first initializes and
  that recorded network applies from then on. Give every node the same
  `--tunnel-network`: a node whose flag differs logs an `ERROR` naming both networks
  and answers registry writes `503`, though an unchanged re-registration is still
  answered `200`. Until the network is recorded (the first seconds of a cluster, or
  while an upgrade waits for a node, docs/12) registry writes answer `503`
  ("cluster is initializing its registries"). An unchanged re-registration proposes
  nothing; an hourly `last_seen` refresh is the only periodic write. Pin-only mode
  works the same way. Upgrade and membership notes: docs/12.
- **Limits.** Transparent mode
  (`transparent: true`) does not apply to tunnel backends whose family differs from the
  client's: the proxy logs a warning and connects without the client's source address.
  Dual-stack tunnels, lease expiry and live address changes are future work (HANDOVER
  "Known follow-ups").

## Open questions

- **What happens when hole-punching genuinely fails** (both the origin's
  and its upstream NAT are restrictive enough that even the
  proxy-has-a-public-IP case doesn't establish a path — rare, but real for
  some consumer ISPs/CGNAT)? Out of scope for v1; the honest fallback is a
  documented limitation, with a relay-of-last-resort (closer to Steam
  Datagram Relay's actual shape) as a possible v2, not designed now.
- **Tunnel-internal address collision/exhaustion** — resolved 2026-10-02: see "Address authority" above.
- **Does this replace or sit alongside the existing "trusted internal
  network" assumption?** Alongside — a same-network deployment still needs
  none of this and keeps working exactly as today; backend transport is
  additive, opt-in per origin.

## Locked decisions (2026-09-05, follow-up session)

- WireGuard interface config: `defguard/wireguard-rs`, not a hand-rolled
  `wireguard-uapi` + `boringtun` split.
- Interface topology: one shared interface with many peers, not one
  interface per origin.
- Config schema: an origin is a `BackendSource` (`backend_sources[].type:
  tunnel`), not a new `origins:`/`pools[].origin` concept.
- `gsp-agent`: its own workspace crate, not a `gsp --agent` mode.
