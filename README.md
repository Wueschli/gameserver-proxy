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

## Status

**Roadmap phases 1–6 complete** (resolver `sticky_key` and
`GET /sessions` deferred). TCP and UDP
listener → backend pool forwarding with:

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
  matcher whose plugins load later (see roadmap Phase 9)
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
  upstream LB without stopping the data path; `GET /config` dumps the live snapshot
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
- security hardening (phase 7, in progress): a filter chain checked before
  routing — per-listener `allow` / `deny` CIDR lists (`deny` wins; non-empty
  `allow` is default-deny) and a `rate_limit` token bucket per source IP and per
  /24 (v4) / /64 (v6) on the client source IP, plus process-wide
  `settings.limits` caps (`max_connections`, `max_udp_sessions`,
  `max_new_sessions_per_sec`); blocked traffic is dropped silently and counted by
  `gsp_filter_blocked_total`

**Phases 1–6 complete** (`sticky_key` and `GET /sessions` deferred). Phase 5
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

cargo run -p gsp -- --config config.example.yaml --check   # validate only
```

Admin endpoints (default `127.0.0.1:9900`): `/healthz`, `/readyz`, `/metrics`,
`/pools`. Log level via `GSP_LOG` (e.g. `GSP_LOG=debug`).

## Workspace layout

| Crate | Responsibility |
|-------|----------------|
| `crates/gsp-config` | YAML config types, parsing, validation (the reduced v0 schema) |
| `crates/gsp-core` | data plane: config snapshot, backend pools, TCP + UDP listeners, byte pump, UDP session tables |
| `crates/gsp` | binary: CLI, logging, admin API, process lifecycle |

## Contributing / continuing the work

- [`CLAUDE.md`](CLAUDE.md) — working agreement, guardrails, "when you touch X also
  touch Y" (written for AI agents; doubles as the contributor reference).
- [`HANDOVER.md`](HANDOVER.md) — current state, locked decisions, deferred items, and
  the concrete plan for the next slice (phase 2, UDP).

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
