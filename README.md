# game-server-proxy

A **game-agnostic game server reverse proxy**: a single entry point in front of
arbitrary game servers that transparently forwards TCP and UDP traffic to backend
instances — without knowing the game's protocol.

Typical goals: hide backend IPs (DDoS protection), port-/hostname-based routing to
many server instances, zero-downtime restarts (connection draining), central metrics
and access control.

## Planning documents

| File | Contents |
|------|----------|
| [docs/00-overview.md](docs/00-overview.md) | Goals, non-goals, use cases, glossary |
| [docs/01-requirements.md](docs/01-requirements.md) | Functional & non-functional requirements |
| [docs/02-architecture.md](docs/02-architecture.md) | Components, data plane / control plane, data flows |
| [docs/03-routing.md](docs/03-routing.md) | Routing strategies in detail |
| [docs/04-transport-and-client-ip.md](docs/04-transport-and-client-ip.md) | TCP/UDP handling, client-IP preservation, PROXY protocol |
| [docs/05-configuration.md](docs/05-configuration.md) | Configuration schema & examples |
| [docs/06-operations-observability.md](docs/06-operations-observability.md) | Metrics, logging, health checks, draining |
| [docs/07-security-ddos.md](docs/07-security-ddos.md) | Rate limiting, ACLs, DDoS mitigation |
| [docs/08-roadmap.md](docs/08-roadmap.md) | Phased implementation / milestones |
| [docs/09-technology-choices.md](docs/09-technology-choices.md) | Language, libraries, alternatives |
| [docs/10-distributed-control-plane.md](docs/10-distributed-control-plane.md) | *(v2, built — phases 10–13)* Fleet-shared config/intent + regional health, controller, aggregator, GUI |
| [docs/11-backend-transport.md](docs/11-backend-transport.md) | WireGuard backend transport for origins behind NAT/a different network (`gsp-agent`, backend/proxy peers registries) |
| [docs/12-deployment.md](docs/12-deployment.md) | Container images, sizes, and how a many-port proxy works under Docker/Kubernetes networking |

## Status

**All roadmap phases 0–14 are built** (resolver `sticky_key` deferred): the data
plane (phases 0–9), the distributed control plane (phases 10–13) and the
WireGuard backend transport (phase 14) — see the sections below and
[docs/08-roadmap.md](docs/08-roadmap.md). Reference container images, a compose demo and
Kubernetes manifests live in [`deploy/`](deploy/). TCP and UDP listener → backend pool forwarding with:

- `round_robin`, `least_conn` and `consistent_hash` (rendezvous-hash affinity,
  `hash_on: src_ip | src_ip_port`) balancing
- active `tcp_connect` / `udp_probe` health checks (`rise`/`fall` thresholds) plus
  passive connect-failure feedback; unhealthy backends are skipped
- UDP: worker-local (lock-free) session tables, one `connect(2)` upstream socket per
  session, `src_ip` / `src_ip_port` backend affinity, idle-timeout eviction, and a
  no-unsolicited-reply amplification guard
- optional per-backend session caps (shared by TCP connections and UDP sessions)
- hot reload on `SIGHUP` or config-file change (atomic snapshot swap; backend health
  carried across the swap)
- admin/observability API and Prometheus metrics
- per-listener route rule list (`routes:`, first match wins) with `always`,
  `client_cidr` (source IP), `dst` (destination IP), `port` (destination port),
  `first_bytes` (prefix and/or length of the first bytes) and `sni` (host from
  the peeked, non-terminated TLS ClientHello) matchers — plus a `sniffer`
  matcher backed by the sandboxed WASM plugin loader (phase 9, below)
- UDP `prefix:` listeners — one wildcard `IP_PKTINFO` socket serves a whole
  routed prefix, routing by the real per-datagram destination and replying from
  it; TCP `freebind:`
- push resolver: `POST /route-hint {src_ip, pool, ttl_sec}` + per-listener
  `route_hint: true` (a short-lived `src_ip → pool` hint wins over the route list)
- operator backend states: `PATCH /pools/{p}/backends/{addr} {state:
  enabled|draining|disabled}` — `draining` / `disabled` divert new sessions while
  existing ones keep running; carried across a reload
- graceful shutdown: `SIGINT`/`SIGTERM` stops accepting and drains in-flight TCP
  connections + UDP sessions, bounded by `settings.shutdown_grace_sec` (default 30)
- instance drain: `POST /admin/drain` / `POST /admin/undrain` flip `readyz` for an
  upstream LB without stopping the data path; `GET /config` dumps the live
  snapshot and `GET /sessions` lists live connections / UDP sessions
- runtime backend CRUD: `POST` / `DELETE /pools/{p}/backends[/{addr}]` add or remove
  a backend at runtime; the edits are layered on the file config and survive a reload
- runtime listener reconfig: a reload spawns added listeners, stops removed ones and
  re-binds changed ones by name — no restart; a same-bind rebind is gapless
- client-IP preservation (phase 6): per-pool `proxy_protocol:
  none | v1 | v2` (TCP) prepends a PROXY protocol header to the upstream
  connection, or `v2-udp` prepends the v2 binary header to the first datagram of
  each UDP session; or a TCP/UDP listener with `transparent: true` (Linux TPROXY)
  sources every upstream connection/datagram from the real client `ip:port` and
  replies from the original destination address
- security hardening (phase 7): a filter chain checked before routing —
  per-listener `allow` / `deny` CIDR lists (radix-trie matched; `deny` wins,
  non-empty `allow` is default-deny), an optional MaxMind GeoIP country filter
  (`settings.geo_db` + per-listener `geo: { allow, deny }`), a `rate_limit`
  token bucket per source IP and per /24 (v4) / /64 (v6), and a `per_source`
  concurrent connection/session cap per IP / /24 / /64 — plus process-wide
  `settings.limits` caps (`max_connections`, `max_udp_sessions`,
  `max_new_sessions_per_sec`); blocked traffic is dropped silently and counted by
  `gsp_filter_blocked_total`. UDP listeners can also set `first_packet_gate: true`
  to open a session only when the first datagram is positively recognised
  (`first_bytes` / sniffer), keeping spoof floods off the session table. The
  amplifier checklist in `docs/07` is covered by `tests/amplification.rs`, the
  peek/config parsers have `cargo-fuzz` harnesses (`make fuzz`), and `make bench`
  (`crates/gsp-bench`) checks the added-latency budget (NFR N1/N2)
- backend discovery (phase 8): a top-level `backend_sources:` list referenced by
  `pools[].source` — `static`, `dns_srv` (SRV), `consul` (health API) or
  `kubernetes` (Endpoints, polled). Level-triggered: each dynamic source has one
  control-plane refresh task that returns the *current* address set, diffed
  into the same snapshot rebuild as file reload / overlay edits; an errored or
  empty refresh keeps the last-known-good set (`gsp_discovery_refresh_total`,
  `gsp_discovery_backends`). Adapters live in the binary; `gsp-core` keeps the
  HTTP-free `BackendSource` seam. A reload reconciles the refresh tasks live
  (`SourceManager`): a `backend_sources[]` entry added / removed / re-parameterised
  starts / stops / restarts its task without a process restart
- sniffer plugin loader (phase 9): `settings.sniffers: { dir, call_timeout_ms,
  max_memory_bytes, modules }` loads `*.wasm` modules — sandboxed `wasmtime`
  (no WASI, no host imports, epoch-interruption time bound + a `StoreLimits`
  memory bound), rescanned live on every config reload. Two first-party
  plugins ship as a separate `crates/plugins/` workspace (`make plugins`):
  `a2s` (Source-engine query recognition) and `minecraft` (virtual-host
  extraction from the protocol handshake), plus a `regex-firstbytes`
  template. Measured comfortably inside NFR N1 (p50 8–10 µs per call,
  real plugins, loopback) — see `docs/07`

**Phase 10+11 complete** (single-tier PoC), **phase 12 complete** (fleet
hierarchy: `standalone`/`slave` controller tiers, intra-tier Raft HA,
staged/canary config rollout, RBAC + audit — see `docs/10`), and **phase 13
complete** (Tier-2 regional health fabric: an authenticated SWIM gossip mesh
per `failure_domain`, sharing per-backend health as a quorum-weighted advisory
signal that only ever nudges — never overrides — an instance's own local
checks; `settings.failure_domain` / `settings.gossip` in `docs/05`). Three
additional binaries alongside `gsp` itself —
`gsp-controller` (a `sled`-backed config revision store: `POST`/`GET /config`,
`GET /config/subscribe` SSE, revision history/diff/rollback — a `gsp
--controller <url>` instance pulls from it instead of a local file, with
freeze-on-disconnect + reconnect-with-backoff), `gsp-aggregator` (fleet reads
+ operational fan-out: instances `POST /ingest` their own state on an
interval, `GET /fleet/pools|sessions|healthz|subscribe` reads it back,
`POST`/`PATCH`/`DELETE /fleet/...` fan intent verbs out to every instance's
own admin API), and `gsp-ui` (a dedicated BFF holding both services' bearer
tokens so the browser only ever needs a session cookie, serving a built
React/Vite/TS frontend). Independent optional bearer-token auth on each hop.
See `docs/06`'s "Fleet control plane" section for the full endpoint
reference, `docs/10` for the design, and `crates/gsp-fleet-tests` for the
multi-process integration tests. The `gsp-ui` frontend was redesigned after the
PoC (Tailwind + Radix, client-side routing, a grouped fleet tree, a
schema-driven settings form with a raw-YAML escape hatch, plugin management);
destructive actions ask for confirmation first, and the frontend has its own
vitest suite (`make ui-test`) — see `docs/08-roadmap.md` "Later / optional" for
what's left.

**Phase 14 complete** (backend transport, `docs/11`): proxies can reach game
servers that are *not* on a shared trusted network. An origin-side
`gsp-agent` brings up a local WireGuard interface (kernel module, `boringtun`
userspace fallback) and registers its public key and fronted backend
addresses with `gsp-controller`'s backend-peers registry (tunnel addresses are
allocated by the controller, `--tunnel-network`); `gsp --tunnel-*`
subscribes to that registry and peers every origin onto its own interface,
and a mirror proxy-peers registry lets agents learn every proxy without a
restart. A `backend_sources[].type: tunnel` source turns a registered origin
into ordinary routable backends, so the data plane is unchanged. Needs
`CAP_NET_ADMIN` + `/dev/net/tun`; container and port-exposure guidance is in
`docs/12`. CGNAT / both-sides-restrictive-NAT is a documented v1 limitation.

Phase 9
(sniffer plugin loader): the WASM sandbox above, plus `crates/plugins/` and
its `README.md`. Phase 8
(discovery & scaling): the `backend_sources` adapters above plus an HA
operations chapter in `docs/06` (anycast vs. L4 LB, per-instance capacity,
dashboards & alerts). Phase 7
(security & hardening): the filter chain above, plus an amplifier-checklist test
suite, `cargo-fuzz` harnesses for the peek/config parsers, and a `make bench`
latency harness for NFR N1/N2. Phase 5
added `draining` / `disabled` backend states, graceful connection draining, the
instance-drain / `GET /config` admin surface, runtime backend add/remove, and
runtime listener add/remove/rebind. External resolver: HTTP + gRPC `resolvers:`
+ `action: { resolver: <name> }`, `pool` / `target` results, `on_error`, and a
TTL'd LRU result cache. Phase 6 is complete: the PROXY protocol header
(TCP `v1`/`v2`, UDP `v2-udp`) and TPROXY transparent mode (`transparent: true`,
TCP + UDP, v4 + v6). See [docs/08-roadmap.md](docs/08-roadmap.md).

## Build & run

Requires a stable Rust toolchain (`rustup` — the repo pins `stable` via
`rust-toolchain.toml`) and `protoc` (the gRPC resolver client is generated at
build time — `apt install protobuf-compiler` / `brew install protobuf`).

```sh
make check                 # fmt check + clippy (-D warnings) + tests
make run                   # run against config.example.yaml
make bench                  # latency / load harness vs. NFR N1/N2
make fuzz                   # parser fuzz targets (needs nightly + cargo-fuzz)

cargo run -p gsp -- --config config.example.yaml --check   # validate only
```

Admin endpoints (default `127.0.0.1:9900`): `GET /healthz` `/readyz` `/metrics`
`/pools` `/config` `/sessions`; `POST /route-hint` `/admin/drain`
`/admin/undrain`; `POST` / `DELETE /pools/{pool}/backends[/{addr}]`;
`PATCH /pools/{pool}/backends/{addr}` (set backend admin state). Log level via
`GSP_LOG` (e.g. `GSP_LOG=debug`).

## Workspace layout

| Crate | Responsibility |
|-------|----------------|
| `crates/gsp-config` | YAML config types, parsing, validation  |
| `crates/gsp-core` | data plane: config snapshot, backend pools, TCP + UDP listeners, byte pump, UDP session tables |
| `crates/gsp` | binary: CLI, logging, admin API, controller/aggregator/tunnel clients, process lifecycle |
| `crates/gsp-controller` | Tier-1 config + operator-intent distribution (`sled` revision logs, SSE, Raft HA, canary rollout, backend/proxy-peers registries) |
| `crates/gsp-aggregator` | fleet-state fan-in (`POST /ingest`, `GET /fleet/*`) and intent-verb fan-out |
| `crates/gsp-ui` | admin GUI BFF (session/RBAC auth) + React/Vite/TS frontend in `web/` (`make ui`) |
| `crates/gsp-agent` | origin-side WireGuard agent (phase 14, `docs/11`) |
| `crates/gsp-fleet-tests` | multi-process integration tests over the real binaries (part of `make check`) |
| `crates/gsp-bench` | latency / load harness (`make bench`) — added p50/p99 vs. NFR N1/N2 |
| `crates/plugins` | first-party WASM sniffer plugins (`a2s`, `minecraft`, `regex-firstbytes`) — standalone workspace, `make plugins` |

## Contributing / continuing the work

- [`AGENTS.md`](AGENTS.md) — working agreement, guardrails, "when you touch X also
  touch Y" (written for AI agents; doubles as the contributor reference).
- [`HANDOVER.md`](HANDOVER.md) — current state, locked decisions, and what's
  deferred / next.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
