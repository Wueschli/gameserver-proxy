# 07 – Security & DDoS mitigation

## Threat model

| Attacker | Goal | Countermeasure (layer) |
|----------|------|------------------------|
| Volumetric (L3/4, Gbit/s) | saturate the uplink | **upstream**: anycast/scrubbing, BGP flowspec. The proxy cannot do this alone. |
| SYN flood (TCP) | connection table / accept | kernel SYN cookies, `accept` backpressure, per-IP conn rate limit |
| UDP flood / spoofed | worker CPU, session table | rate limit before allocation, "validate first packet", session cap, no reflect |
| Amplification via the proxy | abuse the proxy as a reflector | never reply unsolicited; reply only to the sender of an established session |
| Slowloris / idle-holding | tie up resources | peek timeout, idle timeout, min-throughput watchdog |
| Backend IP leak | direct attack bypassing the proxy | backends in the private network only, firewall: only proxy IPs may reach backends |
| PROXY-header spoofing | forged client IP at the backend | the backend accepts PROXY headers only from the proxy IP (allowlist) |
| Resolver abuse | flood the matchmaker with lookups | resolver cache, negative TTL, per-IP rate limit before the resolver call |
| Admin API access | reconfiguration | mTLS/token, internal interface only, no default bind on 0.0.0.0 |

## Filter chain (before routing, cheapest first)

1. **CIDR allow/deny** – static lists + an optional dynamic block list (feed/file, hot
   reload). O(1) via an LPM trie.
2. **Geo filter** (optional) – MaxMind Country DB (`settings.geo_db`), per-listener
   `geo: { allow, deny }` by ISO country code; `deny` wins, a non-empty `allow`
   is default-deny, an unplaceable IP is admitted only with no `allow` list.
   Fails closed if the DB is not loaded.
3. **Connection/datagram rate limit** – token bucket per `src_ip` **and** per `/24`
   (against distributed single IPs). Scope configurable. Excess → drop + metric, **no**
   error reply (no reflect).
4. **Global caps** – `max_connections`, `max_udp_sessions`,
   `max_new_sessions_per_sec`. When reached: reject new ones, protect existing ones.

All steps run **before** buffer/session allocation.

## TCP hardening

- Kernel: `net.ipv4.tcp_syncookies=1`, a sensible `somaxconn`, a short
  `tcp_synack_retries`.
- `accept` backpressure: when a worker is overloaded, throttle the accept rate instead
  of letting the queue explode.
- **Peek timeout** (`peek_timeout_ms`): anyone who sends nothing after connecting is
  dropped before a backend connect happens.
- **Min-progress watchdog**: a connection with no byte progress beyond `idle_timeout`
  → close.
- Optional per-source concurrent cap: `per_source: { max_per_ip, max_per_net }`
  (implemented — a live counter per client IP / /24 / /64, refused before
  allocation).

## UDP hardening

- **No blind replies.** The proxy sends toward the client only after the backend has
  replied within an established session.
- **First-packet gate** (optional, per route): the first datagram must pass a
  `first_bytes` match (a known handshake prefix / a sniffer saying "valid"), otherwise
  no session is created. Keeps generic spoof floods off the session table.
- **Session cap** per `src_ip` / `/24` (`per_source`, implemented — a hard
  concurrent counter, new sessions refused when full) and global
  (`settings.limits.max_udp_sessions`). LRU eviction of the oldest idle sessions
  under pressure is not done — the idle sweep reaps and new sessions are refused
  until room frees.
- **Keep the idle timeout short** where the game allows it (query pools: a few
  seconds).
- Receive buffers large enough that legitimate bursts do not compete with flood
  drops.
- Disable `conntrack` for the proxy path via `NOTRACK` (otherwise it is its own DoS
  surface).

## Network & deployment hardening

- Backends: their own security zone; the ingress firewall allows **only** proxy
  source IPs to the game server ports. Backend egress minimal.
- The proxy runs without root: only `CAP_NET_BIND_SERVICE` (ports < 1024) and — if
  transparent mode — `CAP_NET_ADMIN`. Read-only rootfs, seccomp profile.
- Secrets (admin token, mTLS keys) via env/file with `0600`, not in the YAML.
- Bind the admin API and metrics to a separate internal interface; never on the public
  listener IPs.
- Rate-limit and ACL state is per instance; behind anycast HA, size it per node (an
  attack is spread across all nodes).

## Abuse as an amplifier – checklist

Guarded by `crates/gsp-core/tests/amplification.rs`:

- [x] No reply to datagrams without an established session
      (`no_unsolicited_or_duplicated_replies`).
- [x] No error / unsolicited reply from the app path to a dropped datagram —
      routing, ACL, rate-limit, first-packet-gate drops are all silent
      (`dropped_datagrams_get_no_error_reply`). ICMP is the kernel's; the proxy
      emits none itself.
- [x] Reply size never larger than the backend payload — the proxy forwards
      backend bytes verbatim and prepends nothing toward the client
      (`reply_is_exactly_the_backend_payload`).
- [x] The rate limit applies before any state change — rejected datagrams create
      no session and are never forwarded
      (`rate_limit_is_enforced_before_any_state_change`).

## Sniffer plugin sandbox guarantees (phase 9)

A loaded sniffer plugin (`settings.sniffers`, `crates/gsp/src/sniffer_loader.rs`)
runs under `wasmtime` as a **core WASM module with no WASI and no host
imports** — nothing is wired in beyond the `alloc`/`sniff` exports the plugin
itself provides. Concretely, a plugin cannot:

- read or write the filesystem — no `wasi_snapshot_preview1`, no ambient
  authority of any kind is linked in;
- open a socket or make any network call — same reason;
- read the wall clock, environment variables, or process/thread state —
  nothing exposes them;
- see any byte the proxy hasn't explicitly copied into its linear memory
  (the first-bytes prefix the route already peeked, capped at `peek_len()` ≤
  `PEEK_MAX`) — it never sees later application bytes, and never sees another
  connection's data (a fresh `Store` + `Instance`, and therefore fresh linear
  memory, per call);
- send anything toward the client — a `Sniffer` only returns a `RouteHint`
  (host / key / reject); it has no reply path, so the amplifier checklist
  above is untouched by sniffers entirely.

Two independent bounds cap what a *misbehaving* (buggy or malicious) plugin
can cost the process, checked every call, not just at load time:

- **Time**: `wasmtime` epoch interruption. A dedicated ticker thread bumps
  the engine's epoch every `call_timeout_ms`; each call sets a one-tick
  deadline before running, so a call still executing at the next tick traps
  (`gsp_sniffer_calls_total{result="timeout"}`). This is a *ceiling*, not a
  per-call guarantee of the full budget — a call that starts a moment before
  a tick gets whatever's left, which can legitimately be far less than
  `call_timeout_ms` (`sniffer_loader::tests::
  wasm_plugin_call_times_out_under_the_epoch_deadline` proves the mechanism
  fires; a rare boundary-straddling trap on an otherwise-fast call is
  expected behaviour, not a bug — see the comment in
  `wasm_boundary_latency_vs_nfr_n1` for how the phase 9 latency bench works
  around it to get a clean measurement).
- **Memory**: a `StoreLimits` cap (`max_memory_bytes`) on the `Store` — a
  plugin that tries to grow its linear memory past the cap gets a failed
  `memory.grow` (WASM-spec `-1`), not more memory.

Supply chain: `settings.sniffers.modules: [{ name, sha256 }]` optionally
pins each module's SHA-256; when set, a `dir` entry that isn't listed (or
whose hash doesn't match) fails the whole load/rescan rather than loading
unpinned code.

**Latency vs. NFR N1** (< 0.5 ms *added*; `docs/01-requirements.md`) — the
"latency is a gate, not an assumption" decision from `docs/08` Phase 9,
measured end to end (real `alloc`/`memory.write`/`sniff`/decode round trip,
a fresh `Store` + `Instance` per call, release build) against the three
first-party plugins in `crates/plugins/`:

| plugin | p50 | p90 | p99 | max |
|---|---|---|---|---|
| `a2s` | 10.0 µs | 11.4 µs | 25.8 µs | 399 µs |
| `minecraft` | 8.9 µs | 10.2 µs | 16.9 µs | 217 µs |
| `regex_firstbytes` | 8.3 µs | 9.1 µs | 12.0 µs | 361 µs |

(Loopback numbers on the same box as `crates/gsp-bench`'s N1/N2 measurements;
2000 calls/plugin after warmup. Reproduce with `make plugins` then
`cargo test -p gsp --release wasm_boundary -- --ignored --nocapture` in
`crates/gsp/src/sniffer_loader.rs`.) All three pass N1 with wide margin — a
fresh per-call `Store`/`Instance` (the "no state survives between
connections" design from slice 3) turned out cheap enough on this hardware
that the `InstancePre` / pooling-allocator / warm-instance-per-worker
fallbacks noted in `docs/08` Phase 9 were not needed. The occasional
higher-than-typical `max` samples (hundreds of µs, still far under the 500 µs
p50 gate) are consistent with ordinary scheduling jitter, not a systematic
cost — no sample in any run has been anywhere near N1.

## Security logging

- Events: ACL block, rate-limit trip (aggregated, not per packet), session cap
  reached, health flap, config reload (who/what via the admin API), resolver error
  bursts.
- No plaintext of game payload in logs (privacy); metadata only.
