# 09 – Technology choices

## Language

### Recommendation: Rust
- Predictable latency with no GC pauses — critical for the added RTT (NFR N1/N2).
- Zero-copy data path (`splice`, `sendmmsg`) maps well; fine control over allocations
  (slab, buffer pools).
- Safe parsers for peek/sniffers (no buffer overflow in the exposed path).
- Ecosystem: `tokio` / `monoio` (io_uring), `socket2`, `nix`, `arc-swap`, `hashbrown`,
  `governor` (rate limit), `prometheus`/`metrics`, `rustls` (only if TLS wrapping),
  `tonic` (gRPC resolver).

### Alternative: Go
- Faster to develop, very good networking standard library, easy cross-compilation.
- GC pauses are small today but measurable under millions of sessions; more tuning
  needed.
- Fine if the team is Go-oriented and there is headroom on p99 < 2 ms.

### Not recommended
- C/C++: maximum control, but a safety risk in the exposed parser path.
- Languages with a heavy runtime (JVM, Node) for the data path — only conceivable for
  the control plane.

## Runtime / IO model

| Option | Assessment |
|--------|------------|
| `tokio` (epoll) | mature, broad, good enough; the default choice for v1 |
| `monoio` / `glommio` (io_uring, thread-per-core) | best latency/throughput, `SO_REUSEPORT` + shared-nothing matches the design exactly; younger, smaller ecosystem |
| io_uring directly | maximum control, high effort |

Plan: v1 on `tokio` with a thread-per-core layout (`SO_REUSEPORT`, a LocalSet per
worker). An io_uring backend as a later optimization behind an IO abstraction.

## Key libraries (Rust, proposed)

- **Sockets/syscalls**: `socket2` (bind options incl. `IP_FREEBIND`), `nix` (in
  use for `IP_PKTINFO` recv/reply; later `recvmmsg`, `IP_TRANSPARENT`, `splice`).
- **Data structures**: `hashbrown` (session map), `slab`, `ip_network_table` / an LPM
  trie for ACLs. Consistent hashing is a hand-rolled rendezvous (HRW) hash over the
  healthy backends (`std` `DefaultHasher`) — no `hashring` dependency; the backend
  set is tiny, so HRW's linear scan is cheaper than maintaining a ring.
- **Config**: `serde` + `serde_yaml`, `figment` for env overlay, `notify` for file
  watch.
- **Snapshot swap**: `arc-swap`.
- **Rate limit**: `governor` (GCRA token bucket).
- **Metrics**: `metrics` + `metrics-exporter-prometheus`.
- **Tracing**: `tracing` + `opentelemetry`.
- **Resolver**: `tonic` (gRPC), `reqwest`/`hyper` (HTTP).
- **Admin API**: `axum` (small, on an internal interface).
- **PROXY protocol**: `ppp` or a small custom v2 implementation.
- **Tests**: `criterion` (bench), `cargo-fuzz` (parsers), custom load tools
  (`udp-flood-gen`, `conn-storm`).

## Platform

- **Primarily Linux** (x86-64 + arm64). Kernel ≥ 5.10 for a stable io_uring option.
- Kernel features used: `SO_REUSEPORT`, `splice`, `recvmmsg`/`sendmmsg`,
  `IP_TRANSPARENT`/TPROXY, `TCP_NODELAY`, SYN cookies, `NOTRACK`.
- Container: distroless/scratch image, only the required capabilities.
- No Windows/macOS in the data path (only a dev build with a reduced fast path).

## Architecture decisions (ADR short form)

| ADR | Decision | Rationale | Rejected options |
|-----|----------|-----------|------------------|
| 1 | L4-agnostic core, L7 only as optional read-only sniffers | one proxy for all games; no protocol reverse-engineering in the core | per-game proxy; L7 termination |
| 2 | Thread-per-core + `SO_REUSEPORT`, thread-local session state | no locks on the hot path; linear scaling | a global session map with sharding locks |
| 3 | Immutable config snapshot + atomic swap | reload without blocking the data path | an RWLock on the live config |
| 4 | No shared session state between instances (v1) | complexity/latency; anycast reconnect is enough | Raft/Redis session store |
| 5 | Client-IP preservation optional (PROXY protocol **or** TPROXY) | different backend capabilities / network setups | forcing a single method |
| 6 | External routing logic via a resolver callback + cache | matchmaking stays outside; the proxy understands no tokens | hard-coding routing rules in the proxy |
| 7 | Rust | GC-free latency, safe parsers | Go (GC), C++ (memory safety) |
| 8 | UDP: a `connect(2)` socket per session | return path without a table lookup, kernel sender filtering | one socket + manual demux |
| 9 | UDP v0: one spawned reply-pump task per session; idle expiry by a 1 s sweep; sticky table bounded by a hard cap and cleared wholesale | ships the vertical slice without `recvmmsg`/timing-wheel/LRU machinery; each is a drop-in later | building the batching + timing wheel + LRU up front |
| 10 | `dst` prefix listener: one wildcard `IP_PKTINFO` / `IPV6_RECVPKTINFO` socket per prefix, real dest per datagram, reply source via `sendmsg` cmsg — using `nix` (safe wrappers) | keeps the zero-`unsafe` invariant; `nix` was already slated for `recvmmsg` / TPROXY / `splice` | one socket bound per address (does not scale); raw `libc` + `unsafe` cmsg walking |

## Risks & mitigations

| Risk | Mitigation |
|------|------------|
| io_uring portability/bugs | an abstraction, `tokio`/epoll as the default backend |
| the `splice` fast path does not cover all cases (TLS peek leftover bytes) | a clean fallback buffered path, property tests |
| the session table as a memory DoS | hard caps + LRU eviction + first-packet gate |
| sniffer parsers as an attack surface | `#![forbid(unsafe)]` in plugins, fuzzing, byte/time limits |
| TPROXY network setup is error-prone | thorough docs + a `preflight check` command |
| resolver latency in connection setup | cache, tight timeout, `stale_ok`, fallback route |
