# Component versioning and rolling upgrades design (#185)

Status: design for review. Phase A (version fields) is planned in `docs/superpowers/plans/2026-10-05-protocol-and-config-versions.md`; Phase B (this design's behaviour) in `2026-10-05-component-upgrades.md`.

## Goal

An operator can upgrade a fleet (controllers, aggregator, UI, proxies, agents) one component at a time without a flag day, and a node that cannot talk to its peer says so plainly instead of misbehaving.

## What is versioned

| Surface | Carrier | Scheme |
|---|---|---|
| Component HTTP/JSON, SSE and raft routes | `X-Wayhouse-Protocol` header | `major.minor`, starts `1.0`, independent of the product version (stays 0.x) |
| Gossip (UDP, postcard, HMAC) | one version byte inside the MAC | protocol major |
| Config document | `schema_version` | integer, starts 1 |
| Controller store (sled) | `meta/format` | integer, starts 1 |
| Plugin ABI | `wayhouse.abi` custom section | `0.1`, exact match while major is 0 |
| External resolver gRPC | proto package `wayhouse.resolver.v1` | already versioned by package name |
| Product | `[workspace.package] version`, `wayhouse_build_info` | SemVer 0.x |

Not covered, listed so nobody assumes otherwise: the raft log storage (`ha/log_store.rs`) and snapshot format beyond the store marker; the WireGuard tunnel itself.

## Compatibility rules

- **Protocol major** differs: refuse (`426`, gossip datagram dropped and counted). A change is *breaking* (major bump) if an older peer would misread or fail on it: removed or retyped field, changed semantics, new required field.
- **Protocol minor** is additive only: new optional fields and new routes. Receivers ignore unknown JSON fields (serde default; verify every component-facing struct does not set `deny_unknown_fields`; config is the deliberate exception).
- **Window: N / N-1 product minor versions** (0.N and 0.(N-1)) are supported together; this is what the protocol majors must honour: do not bump the protocol major twice within two product minors without a compatibility shim, or the window is a lie. CI enforces it with a fleet test (below).
- Config: a node refuses a `schema_version` newer than it supports; an older document is always accepted (new fields have defaults).
- Store: a node refuses a newer `format`; migrating older formats on open is allowed and done once.

## Order of operations (recommended)

1. **Aggregator and UI** (read side) first: they tolerate older proxies and controllers within the window.
2. **Controllers**, HA cluster: upgrade the slave (follower) first, let it catch up, fail over (HA runbook), then upgrade the former master. A newer controller must not break an older proxy: new fields are only sent to a peer whose advertised protocol minor understands them (every response carries the server's `X-Wayhouse-Protocol`, and the client remembers the last value it saw; an unknown peer gets the baseline).
3. **Proxies and agents**, a few at a time: drain, upgrade, rejoin (`--drain` / admin drain, then restart; existing connection draining keeps players connected where the proxy supports it).
4. Kubernetes: the same order via `maxUnavailable: 1` rolling updates and readiness gating (ties into #88).

This keeps the transition plan's default (controllers before proxies); the response-header echo that Phase A adds is what makes it safe.

## Visibility

The UI fleet view shows each node's `wayhouse_build_info` version and protocol (`wayhouse_build_info` gains a `protocol` label in Phase B) and flags skew: yellow when versions differ within the window, red when outside it or a mismatch counter (`wayhouse_protocol_mismatch_total`) is non-zero.

## Testing

A fleet test in `crates/wayhouse-fleet-tests` that starts a controller at the current version and a proxy built with an injected older protocol minor (a test-only env override of `PROTOCOL_MINOR`) and asserts: works, no newer fields sent; and with a different major: refused with a clear log line and the mismatch counter incremented. A docs test (`docs/upgrading.md` must list every protocol version bump in a table; checked by a small script against `version.rs`).

## Open questions

- Whether raft needs its own version gate beyond the header on `/raft/*` routes (the openraft RPC payload types change with the crate version).
- Whether the window should be one product minor or two; this design says N/N-1 as proposed in #185; revisit at the first real incompatible change.
