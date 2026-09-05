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

## Key libraries (Rust)

As-built, not the original pre-implementation shortlist — several items below
were planned to use an external crate and shipped as a small hand-rolled
equivalent instead, once the actual shape of the problem (tiny backend sets,
one map per worker, no need for a general-purpose engine) made the extra
dependency not worth it. Still-open v2 choices are marked as such.

- **Sockets/syscalls**: `socket2` 0.6 (bind options incl. `IP_FREEBIND`,
  `IP_TRANSPARENT` v4/v6 via `SockRef`), `nix` (`IP_PKTINFO` / `IP_ORIGDSTADDR`
  recv + reply cmsgs; `recvmmsg` UDP ingress — ADR 18; `splice` TCP pump — ADR
  17). `sendmmsg` UDP egress still to do.
- **Data structures**: plain `std::collections::HashMap` for the UDP session
  table (worker-local, no sharding needed — `hashbrown` was considered but adds
  nothing at this scale). ACLs are a hand-rolled binary radix trie over address
  bits (`gsp_config::CidrSet`) rather than the `ip_network_table` crate — kept
  the `gsp-config` "serde + thiserror only" dependency rule intact. Consistent
  hashing is a hand-rolled rendezvous (HRW) hash over the healthy backends
  (`std` `DefaultHasher`) — no `hashring` dependency; the backend set is tiny,
  so HRW's linear scan is cheaper than maintaining a ring. `weighted` is
  likewise a plain weighted round-robin over the cumulative-weight line (one
  atomic tick per selection, no smooth-WRR per-backend state) — blocky ordering
  is fine at this backend-set size.
- **Config**: `serde` + `serde_yaml`, `notify` for file watch. No env-var
  overlay exists (the `figment` idea from the original plan was dropped —
  the YAML file + `SIGHUP`/watch reload covers the actual need).
- **Snapshot swap**: `arc-swap`.
- **Rate limit**: a hand-rolled token bucket (`gsp_core::ratelimit`, a
  `Mutex<HashMap<key, Bucket>>`, monotonic refill) rather than `governor` — one
  bucket type, two key shapes (per-IP / per-net), not worth a general GCRA
  dependency.
- **Metrics**: `metrics` + `metrics-exporter-prometheus`.
- **Tracing**: `tracing` + `tracing-subscriber`. No `opentelemetry` exporter —
  `/metrics` (Prometheus) is the only telemetry sink today.
- **Resolver**: `reqwest` (HTTP) + `tonic`/`prost` (gRPC), both in the `gsp`
  binary only; `gsp-core` defines the `Resolver` trait. gRPC codegen via
  `tonic-build` at build time (needs `protoc`).
- **Admin API**: `axum` (small, on an internal interface).
- **Distributed control plane (v2, ch. 10)**: Tier-1 store — the phase 10+11
  PoC's embedded `sled` (ADR 20) is **kept** for HA too, replicated via
  embedded **`openraft`** (ADR 21) rather than switched to `etcd` or git;
  Tier-2 health gossip — `foca` (SWIM) or a hand-rolled `(instance, backend)`
  LWW-CRDT sync, still undecided. RBAC password hashing — **`argon2`** (ADR
  23, design only). All in `gsp-controller` / `gsp-ui` / the `gsp` binary,
  never `gsp-core`. RBAC (ADR 23) is the one item in this bullet still
  **design only** — the hierarchy, both relay logs, adoption, HA (ADR 21),
  and staged/canary rollout (ADR 22) are all built — `docs/08-roadmap.md`.
- **PROXY protocol**: a small custom v1/v2 encoder (`gsp_core::proxy_protocol`)
  — no `ppp` dependency; write-only (the proxy never parses an inbound header)
  made the hand-rolled version simpler than adopting a parsing-capable crate.
- **Tests**: a custom `Stats`/percentile harness (`gsp-bench`) rather than
  `criterion` — matches the project's own latency-ledger reporting shape
  instead of criterion's statistical-comparison model; `cargo-fuzz` for the
  untrusted-input parsers (`crates/gsp-config/fuzz/`).

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
| 16 | *(phase 9)* Sniffer plugins run as **`wasmtime` core modules with no WASI** and a 3-function ABI (`memory`, `alloc`, `sniff(…)->packed u64` returning an encoded `RouteHint`; the `sniff` signature is widened by ADR 16a). Bounds: epoch-interruption deadline (`call_timeout_ms`, host ticker thread) + `StoreLimits` memory cap; input already ≤ the route's `peek_len()`. `wasmtime` is a `gsp`-binary dep; the `Sniffer` trait + a runtime-threaded `Arc<Sniffers>` registry stay in `gsp-core`. First-party `a2s` / `minecraft` / `regex-firstbytes` plugins ship built the same way. | game-specific parsing must never be in the agnostic core or a fork; no WASI ⇒ a plugin can't reach fs / clock / net; epoch interruption pre-empts a runaway parser; the registry seam mirrors resolvers so `gsp-core` gains no plugin runtime | compile sniffers in (fork per game); `extism` (heavier, less control over fuel/epoch); the component model + WASI (larger surface, bigger dep); an out-of-process sniffer per connection (latency — kept only as the fallback if the in-process boundary misses NFR N1) |
| 16a | *(phase 9, amends ADR 16)* **Per-plugin config via a widened `sniff` signature.** `sniff(in_ptr, in_len)` → `sniff(in_ptr, in_len, cfg_ptr, cfg_len)`; the host `alloc`s + writes the module's `settings.sniffers.modules[].config` string into a second linear-memory region on every call (a fresh `Store` per call, so it is re-marshalled each time), `cfg_len == 0` when unset. `config` is baked onto the `WasmSniffer` at load/rescan time; the `gsp-core` `Sniffer` trait is unchanged (config is host-side). | the ABI is a third-party contract that freezes at 1.0 and there are **zero** external plugins today, so the clean break is at its cheapest now; a uniform 4-arg signature (config always passed) has no "call `configure` if the module exports it" branch and no two-class plugin model; passing config as a second `(ptr,len)` region reuses the exact mechanism the input already uses | an optional `configure(ptr,len)` export (compat-shim shape the pre-1.0 guardrail rejects; permanent host branch; two plugin classes); a length-prefixed cfg blob prepended to the input region (overloads one region with two meanings; every plugin must parse the framing even to ignore it); config-without-pin as a first-class path (deferred — a `modules[]` entry still requires its `sha256`) |
| 17 | *(perf pass)* **TCP pump uses `splice(2)` on Linux**, behind the same `proxy::pump` fn. Each direction owns one `O_NONBLOCK` pipe pair for the connection's life; bytes move `socket → pipe → socket` via `nix::fcntl::splice` (feature `zerocopy`) driven by `TcpStream::try_io` readiness. Non-Linux — and a `pipe2` failure — fall back to the buffered `try_read` / `try_write` loop (also over `&TcpStream`, no `into_split`). Half-close is propagated with `socket2` `SockRef::shutdown(Write)`. | no userspace copy and no 32 KiB×2 per-connection heap buffers on the steady path — the win is CPU / memory-bandwidth / RSS at high connection counts (gates N3–N5), not loopback RTT; drop-in behind the existing fn so every forwarding test still covers it | keep the buffered copy (a userspace bounce per byte); `sendfile` (file→socket only); `io_uring` (bigger dep + runtime — revisit if the pipe hop shows up in a profile); one shared pipe for both directions (couples the two directions' backpressure) |
| 18 | *(perf pass)* **UDP ingress batches with `recvmmsg(2)`** (`nix`, Linux) — one syscall per readiness wakeup pulls up to `RECV_BATCH` (16) datagrams into a per-worker `RecvBatch` (16 × 64 KiB buffers, ~1 MiB/worker); the existing per-datagram route / filter / forward loop then runs over the batch. A fresh `MultiHeaders` per call (nix does not reset `msg_namelen` / `msg_controllen` between calls, so reuse would truncate a v4→v6 address or a cmsg). Non-Linux keeps one `recvmsg` per call behind the same `RecvBatch` API. The prefix / transparent dest cmsg parse moved into the batch path (`dst_from_cmsg`). | the listen socket is the single contended c2s funnel — amortising its recv syscall is the highest-leverage UDP change; buffers are per-worker not per-session, so no RSS blow-up; drop-in behind `RecvBatch` so every UDP forwarding test still covers it | `sendmmsg` egress batching in the same slice (per-session reply buffers would 16× the RSS, or force per-datagram allocs — deferred as its own change); reusing one `MultiHeaders` (address / cmsg truncation footgun); `io_uring` multishot recv (bigger dep) |
| 19 | *(perf pass)* **UDP idle expiry is a single-level timing wheel** (`IdleWheel`, `WHEEL_SLOTS`=512 one-second slots). A session is filed in the slot for its idle-deadline second; a tick drains one slot, evicting entries whose live `last_ms` shows real idleness and re-filing the rest (a datagram moved the deadline, or `idle_ms` > the wheel span). Exactly one entry per live session. The per-datagram hot path is unchanged — it still only bumps the atomic `last_ms`. | tick cost drops from O(sessions) (the old 1 s `retain` scan) to O(slot + re-files); activity refresh stays lazy so the datagram path pays nothing; a too-long `idle_ms` degrades to a re-check every `WHEEL_SLOTS` s, never a miss | per-datagram wheel re-insertion (moves work onto the hot path); a hierarchical wheel (more code, unneeded at 1 s granularity / ~9 min span); a `BinaryHeap` by deadline (O(log n) per op, worse cache behaviour) |
| 20 | *(v2 PoC, ch. 10)* **Tier-1 store: an embedded KV (`sled`), single-node, in `gsp-controller`.** Each accepted config submission gets a monotonic revision key; subscribers get a full snapshot + cursor, then a change stream over the same store. No Raft/etcd for this first release — the controller is a single `standalone` node with no HA yet (see roadmap "PoC" note). | a proper KV beats pushing config files around (crash-consistent, queryable by revision, no ad hoc file-locking) while staying a pure Rust, zero-ops embedded dep — appropriate for a single-node PoC; swapping the backing store for a distributed one (etcd, or Raft over the same KV) is an additive later-release change behind the same store trait, not a rewrite | etcd/Consul (an extra service to run for a PoC that is explicitly single-node); a flat versioned JSON file (no crash-consistent multi-key transactions, reinvents what a KV already gives); embedding Raft now (HA is explicitly deferred past this release) |
| 21 | *(phase 12 slice 6, built — `gsp-controller` only; the aggregator's HA design further down stays design-only)* **Intra-tier controller HA: embedded `openraft` 0.9, one Raft group per tier replicating both the config and intent logs together, `sled` unchanged as the state machine.** Writes propose a Raft entry and only commit on the leader; a non-leader replica transparently HTTP-forwards a write to the current leader rather than redirecting. Reads are served by whichever replica got the request, straight from its local `sled` — no `ReadIndex` linearizable-read protocol. Cluster membership is static at bootstrap (`--ha-peers`, identical on every replica — each calls `raft.initialize()`, a harmless no-op on every node but whichever wins the race); dynamic membership change is deferred. **Scope cut, not yet built**: a `slave` tier's upward relay running leader-only with a replicated cursor — HA and `--role slave` are mutually exclusive for now (rejected at startup). Raft RPCs ride the same `axum` server on a `/raft/*` prefix gated by a new peer-only `--ha-token`. | continues ADR 20's "embed, don't run an extra service" story instead of trading it away the moment HA enters the picture — `etcd` would add real operational machinery for a component whose write volume is human-paced, not a hot path; a hand-rolled Raft would reimplement exactly the subtlety (log matching, leader lease safety, snapshotting) a maintained, audited crate already gets right for a security-sensitive component; relaxed reads are justified by the same "freeze on last-known-good" principle (4) the whole chapter already leans on — a follower a few commits behind is the same acceptable staleness as a subscriber on its last replica during an outage, never a divergent one, since `sled`'s local apply only ever appends; transparent forwarding keeps every existing client (`gsp`, `gsp-ui`, `curl`) ignorant that HA exists at all | `etcd` as the Tier-1 backing store (extra service, operational drift from ADR 20's own reasoning); a hand-rolled Raft/Paxos; two independent Raft groups (config, intent) per tier (two leader elections, two peer lists, possible disagreement about who leads); an HTTP redirect to the leader instead of a transparent forward (pushes leader-awareness onto every client); `ReadIndex` linearizable reads (real cost for no benefit this design already doesn't need); dynamic membership on day one (real complexity deferred until a deployment actually needs to resize a live group) |
| 22 | *(phase 12 slice 7, built)* **Staged / canary config rollout: one monotonic revision log (never forked), each revision tagged in a separate `stage` `sled` tree with `{promoted, canary_groups}`.** An instance self-reports an optional `canary_group` when subscribing; "current for group G" = the highest revision that is `promoted` or has `G` in `canary_groups`, computed by an O(revisions) backward scan rather than a maintained index. `POST /config/promote/{revision}` flips `promoted` in place — the one deliberate exception to "every change is a new revision," since promotion changes visibility of existing content, not the content itself. Canary staging applies to config only, never the intent log. | one log avoids ever having to reconcile two branching histories later, which a forked-log design would eventually force; self-reported group membership matches the trust level every other self-reported fact in this control plane already has (`IngestPayload::admin_url`, an instance's own `--role`); an O(revisions) scan is proportionate to a human-paced, hundreds-to-low-thousands-of-entries log — the same "don't build for a scale problem that doesn't exist yet" bias the rest of this codebase already uses; intent ops are already narrowly single-scoped (one backend), so "stage this whole document to a subset" has no equally clean meaning for "stage this one op" | a forked/branching revision history per canary group (real merge/reconciliation problem, for a feature meant to be simple); a controller-verified (not self-reported) group membership (real feature, not needed yet, ties into the still-open region-scoped-intent question); an index-backed "current per group" lookup (premature at this scale); extending canary staging to the intent log (no clean partial-rollout semantics for a single op) |
| 23 | *(phase 12, design only)* **RBAC lives in `gsp-ui` (the only human-facing process), not the controller/aggregator.** `--users-file` (username + `argon2` password hash + one of three roles — `viewer`/`operator`/`admin` — matching the API surface's existing verb tiers) replaces `--ui-password` for multi-operator deployments; `--ui-password` is kept as a legacy single-shared-secret (implicitly `admin`) mode rather than removed. `SessionStore` maps a session id to a `Role`; a new `require_role(min)` middleware layers per route group next to `require_session`. Audit: `gsp-ui` adds `X-Actor: <username>` to every write it proxies; `gsp-controller` records it per revision in a new `actors` `sled` tree (`GET /config/revisions` gains an `actor` field) — a durable per-revision attribution, not a separate durable audit log. | keeps the phase 10+11 slice-11 divergence from `docs/10`'s original "RBAC lives on the controller" text consistent, rather than half-reverting it — the controller/aggregator still gate on one shared machine token each, exactly as built; `argon2` is the currently-conventional choice for this exact job, the same "defer to a maintained crate for a security-critical, well-solved problem" reasoning ADR 16 already used for sniffer sandboxing; three roles match every route in the existing surface without inventing a permission matrix nothing here needs yet; per-revision `actor` reuses the controller's already-durable revision history instead of standing up a second audit-log store for a three-role, single-human-facing-process model | RBAC enforced on the controller/aggregator (couples authz to the machine-token model those two intentionally kept simple); a general permission-matrix / policy-engine model (solves a problem this API surface's three natural tiers don't have); forcing every existing single-operator deployment onto `--users-file` (pure friction, no correctness upside below two identities); a separate durable audit-log service (disproportionate to this model's size) |

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
| *(phase 12, design)* a lost Raft quorum in an HA controller tier | degrades to the same "changes frozen for this subtree" behavior a single-node outage already has (ADR 21) — never a new failure mode, just a bigger blast radius to lose quorum on |
| *(phase 12, design)* a canary revision never gets promoted and silently rots | it stays visible only to its declared `canary_group` forever, invisible to `GET /config/revisions`' *promoted* pointer for everyone else — an operator problem (forgot to promote or roll back), not a correctness one (ADR 22) |
| *(phase 12, design)* a `gsp-ui` operator account's password hash leaks | `argon2` (a deliberately slow KDF) bounds offline cracking cost; role scope still limits blast radius to whatever that account's role permits, never full fleet authority unless the leaked account was `admin` (ADR 23) |
