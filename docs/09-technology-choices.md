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

- **Sockets/syscalls**: `socket2` 0.6 (bind options incl. `IP_FREEBIND`,
  `IP_TRANSPARENT` v4/v6 via `SockRef`), `nix` (`IP_PKTINFO` / `IP_ORIGDSTADDR`
  recv + reply cmsgs; later `recvmmsg`, `splice`).
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
- **Resolver**: `reqwest` (HTTP) + `tonic`/`prost` (gRPC), both in the `gsp`
  binary only; `gsp-core` defines the `Resolver` trait. gRPC codegen via
  `tonic-build` at build time (needs `protoc`).
- **Admin API**: `axum` (small, on an internal interface).
- **Distributed control plane (v2, ch. 10)**: Tier-1 store — embedded Raft
  (`openraft`) *or* an `etcd` client *or* git, decision deferred; Tier-2 health
  gossip — `foca` (SWIM) or a hand-rolled `(instance, backend)` LWW-CRDT sync.
  All in `gsp-controller` / the `gsp` binary, never `gsp-core`.
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
| 4a | *(v2, ch. 10)* Config **and** operator intent become fleet-shared via an ordered durable store (Tier 1); backend **health** becomes shared **within a failure domain** via gossip (Tier 2). **Sessions stay unshared** — ADR 4 holds for sessions. | operators need one authoritative config, persisted intent, and multi-vantage health; sessions do not benefit enough to justify the coupling | one global store for everything (cross-region health is noise); keep everything per-instance (intent lost on restart, not fleet-consistent) |
| 5 | Client-IP preservation optional (PROXY protocol **or** TPROXY) | different backend capabilities / network setups | forcing a single method |
| 6 | External routing logic via a resolver callback + cache | matchmaking stays outside; the proxy understands no tokens | hard-coding routing rules in the proxy |
| 7 | Rust | GC-free latency, safe parsers | Go (GC), C++ (memory safety) |
| 8 | UDP: a `connect(2)` socket per session | return path without a table lookup, kernel sender filtering | one socket + manual demux |
| 9 | UDP v0: one spawned reply-pump task per session; idle expiry by a 1 s sweep; sticky table bounded by a hard cap and cleared wholesale | ships the vertical slice without `recvmmsg`/timing-wheel/LRU machinery; each is a drop-in later | building the batching + timing wheel + LRU up front |
| 10 | `dst` prefix listener: one wildcard `IP_PKTINFO` / `IPV6_RECVPKTINFO` socket per prefix, real dest per datagram, reply source via `sendmsg` cmsg — using `nix` (safe wrappers) | keeps the zero-`unsafe` invariant; `nix` was already slated for `recvmmsg` / TPROXY / `splice` | one socket bound per address (does not scale); raw `libc` + `unsafe` cmsg walking |
| 11 | Graceful shutdown drain: a `watch<usize>` connection counter (`ConnTracker` / `ConnGuard`) + `wait_for(==0)`, bounded by `shutdown_grace_sec`; UDP recv loop enters a drain state instead of returning | no new dep; race-free wait; per-conn cost is one `send_modify` on open/close, nothing per byte | `tokio_util::task::TaskTracker` + `CancellationToken` (extra dep); a global `Mutex<HashSet<JoinHandle>>` |
| 12 | Transparent mode (TPROXY): per-listener `transparent: bool`. TCP — `IP_TRANSPARENT` on the listen socket + a client-`ip:port`-bound `IP_TRANSPARENT` upstream `TcpSocket`. UDP — `IP_TRANSPARENT` + `IP_RECVORIGDSTADDR` on the listen socket (`recvmsg` for the original `ip:port`), a client-bound `IP_TRANSPARENT` upstream socket, and a per-session `IP_TRANSPARENT` reply socket bound to the original destination. `IP_TRANSPARENT` via `socket2` `SockRef` (bumped 0.5 → 0.6 for `set_ip_transparent_v6`), origdst cmsg via `nix` — still zero `unsafe` | one flag turns on the whole path; the reply socket restores source `ip:port` exactly (a pktinfo cmsg can only set the source IP, not the port); socket2 0.6 was already in the tree via `hyper-util` | PROXY protocol only (needs backend support); an `IPV6_TRANSPARENT` raw `libc` + `unsafe` `setsockopt`; reusing the prefix-mode `sendmsg` cmsg reply path (wrong source port) |
| 12a | Backend discovery: a **level-triggered** `BackendSource` seam in `gsp-core` (`fetch → Vec<SocketAddr>` = the current set; the runtime diffs it, like listener reconcile), one control-plane `refresh_loop` task per source, results fed through `Snapshot::build_with_sources` on the existing reload path (one snapshot writer). Concrete clients (`hickory-resolver` for SRV, `reqwest` for Consul / k8s) in the `gsp` binary; k8s **polls** Endpoints. An errored / empty refresh keeps the last-known-good set. | matches the immutable-`Snapshot` invariant and lets a Tier-1 store (ch. 10) reuse the same reconcile; degrade-in-place instead of draining a pool on a discovery blip; HTTP / DNS deps stay out of `gsp-core` | add/remove deltas from each source (drift-prone); a k8s watch informer (deferred — more code, marginal gain at this fleet size); clearing the pool on an empty result |
| 13 | *(v2, ch. 10)* Level-triggered, **pull-based** config distribution: instances subscribe to the Tier-1 store, receive a snapshot + revision cursor + change stream, and feed each revision through the **existing** `validate() → Snapshot::build → ArcSwap::store` path. The store is one more writer; `gsp-core` is untouched, the client lives in the `gsp` binary (same seam as resolvers). | an instance offline for a while catches up from its cursor with no writer-side delivery state; a bad revision is rejected exactly like a bad file reload | push distribution (needs per-instance delivery tracking); a new data-plane config transport replacing the file |
| 14 | *(v2, ch. 10)* A **separate** `gsp-controller`: Tier-1 writer + fleet read-aggregator + the GUI's only backend + all authn/authz/audit. Proxies never peer through it. HA = N replicas behind a leader lock for writes; its outage freezes *changes*, not traffic. | keeps consensus / auth off the data-plane nodes; one place to secure and audit | leader election among the proxy instances themselves; auth on every proxy admin API |
| 15 | *(v2, ch. 10)* Regional health authority: local active/passive checks decide; the failure-domain gossip view is **quorum-weighted advice** — unhealthy on local `fall` **or** domain quorum-down, healthy **only** on local `rise`, Tier-1 `force-down` overrides. | one flapping instance can't poison the pool (quorum gate on "down"); one instance can't ignore a domain-wide outage (the `or`); a stale remote "up" can't revive a locally-unreachable backend | trust the shared verdict outright; share health globally (noise across reachability classes) |
| 16 | *(phase 9)* Sniffer plugins run as **`wasmtime` core modules with no WASI** and a 3-function ABI (`memory`, `alloc`, `sniff(ptr,len)->packed u64` returning an encoded `RouteHint`). Bounds: epoch-interruption deadline (`call_timeout_ms`, host ticker thread) + `StoreLimits` memory cap; input already ≤ the route's `peek_len()`. `wasmtime` is a `gsp`-binary dep; the `Sniffer` trait + a runtime-threaded `Arc<Sniffers>` registry stay in `gsp-core`. First-party `a2s` / `minecraft` / `regex-firstbytes` plugins ship built the same way. | game-specific parsing must never be in the agnostic core or a fork; no WASI ⇒ a plugin can't reach fs / clock / net; epoch interruption pre-empts a runaway parser; the registry seam mirrors resolvers so `gsp-core` gains no plugin runtime | compile sniffers in (fork per game); `extism` (heavier, less control over fuel/epoch); the component model + WASI (larger surface, bigger dep); an out-of-process sniffer per connection (latency — kept only as the fallback if the in-process boundary misses NFR N1) |

## Risks & mitigations

| Risk | Mitigation |
|------|------------|
| io_uring portability/bugs | an abstraction, `tokio`/epoll as the default backend |
| the `splice` fast path does not cover all cases (TLS peek leftover bytes) | a clean fallback buffered path, property tests |
| the session table as a memory DoS | hard caps + LRU eviction + first-packet gate |
| sniffer parsers as an attack surface | *(phase 9)* run as `wasmtime` core modules, no WASI; epoch-deadline + memory cap + input ≤ `peek_len`; `#![forbid(unsafe)]` + fuzzing in the plugin crates; `sha256` pinning |
| *(phase 9)* WASM call latency blows the NFR N1 budget | measure the boundary before wiring it on the per-connection path; `InstancePre` / warm per-worker instance; out-of-process resolver as the fallback |
| *(phase 9)* `wasmtime` dependency size / build time | binary-only dep; engine cache in CI; `wasm32-unknown-unknown` target added once |
| TPROXY network setup is error-prone | thorough docs + a `preflight check` command |
| resolver latency in connection setup | cache, tight timeout, `stale_ok`, fallback route |
| *(v2)* a bad Tier-1 revision blackholes the whole fleet | `validate()` on controller **and** instance; signed revisions; instance-side sanity bound (pool `> 0 → 0` targets ⇒ warn + keep); canary rollout; one-key rollback |
| *(v2)* Tier-1 store / controller outage | instances serve their last local replica indefinitely; only *changes* stop |
| *(v2)* rogue host injects "all backends down" into the health fabric | Tier-2 is advisory + locally checked (never revives on remote "up"); signed / mTLS gossip mesh |
