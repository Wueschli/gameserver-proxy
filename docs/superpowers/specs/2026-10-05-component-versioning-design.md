# Component versioning and rolling upgrades design (#185)

Status: design for review (brainstormed 2026-10-05). Phase A (version fields) is planned in `docs/superpowers/plans/2026-10-05-protocol-and-config-versions.md`; Phase B (this design's behaviour) in `2026-10-05-component-upgrades.md`.

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
- Config: `schema_version` is the **minimum schema a document needs**; absent means 1. The bump rule: any PR that adds a config field (even optional) bumps `CONFIG_SCHEMA_VERSION`, records the field's `since` version in a `FIELD_SINCE` table in `wayhouse-config`, and the parser rejects a document that uses a field newer than its declared `schema_version` (so an operator cannot use a new field by accident). `deny_unknown_fields` stays: an older node rejects a newer field with a clear error rather than ignoring it. A node refuses a `schema_version` above what it supports. Proxies report their `max_config_schema` at registration; the controller refuses (`422`) a submitted config whose `schema_version` exceeds the lowest value any registered proxy reported, so a mixed fleet never receives a document it must reject. Operator rule while the fleet is mixed: upgrade every proxy before using a field introduced by the new version (config is the one surface where proxies go before the controller).
- Store: a node refuses a newer `format`; migrating older formats on open is allowed and done once.

## Order of operations (recommended)

There are three different mechanisms here; the runbook keeps them apart.

**Gating rule that makes any order inside the window safe.** The sender of data gates new optional fields by the receiver's advertised protocol minor. Both directions carry it: every request carries the client's `X-Wayhouse-Protocol` (the server records it per caller and per SSE stream on connect, exposed to handlers as a request extension), and every response carries the server's (the client remembers the last value). A peer with no known version gets the baseline. This covers controller to proxy, parent tier to slave tier (`parent_client` and `intent::relay` run the other way: the slave is the HTTP client, the parent sees its header), proxy to aggregator and agent to controller.

1. **Aggregator and UI** (read side) first.
2. **Controller tiers.** A `slave` tier is a child controller under a parent (`role.rs`: writes arrive via `parent_client` and `relay`); upgrade the leaf slave tiers first, then work up to the root tier. Within a window either order works because of the gating rule; leaves-first keeps the root, which owns writes, on the version the operators have tested longest.
3. **HA inside one tier (raft).** A tier made of several controllers has a raft leader and followers (`ha/`; not master and slave). Upgrade followers one at a time, let each catch up, move leadership with the existing HA runbook, then upgrade the former leader.
4. **Proxies and agents**, a few at a time: drain, upgrade, rejoin (existing connection draining keeps players connected where the proxy supports it; the UDP draining bug #186 must be fixed first).
5. Kubernetes: the same order via `maxUnavailable: 1` rolling updates and readiness gating (ties into #88).

Config documents are the exception to "controllers first": see the config rule above.

## Visibility

The UI fleet view shows each node's `wayhouse_build_info` version and protocol (`wayhouse_build_info` gains a `protocol` label in Phase B) and flags skew: yellow when versions differ within the window, red when outside it or a mismatch counter (`wayhouse_protocol_mismatch_total`) is non-zero.

## Testing, and what is and is not enforced

- A fleet test in `crates/wayhouse-fleet-tests` starts a controller and a proxy with an injected older protocol minor (test-only override) and asserts additive behaviour: it works and no newer fields are sent. A second test uses a different major and asserts refusal, the readable log line and the mismatch counter.
- The N / N-1 **product** window is not enforced by the header check (majors only; protocol versions are independent of product minors). Until two releases exist it is a documented rule kept by review (the `AGENTS.md` row for wire changes). Once `v0.1.0` and a later release exist, a CI job (`compat`, not required) runs the previous release's images against the current ones in the compose demo and the fleet smoke test. Do not claim more than that in the docs.
- A script test checks that the newest row of the version table in `docs/upgrading.md` equals the constants in the code.

## Decisions and settled questions

- **[decided 2026-10-05, by decision card in the project thread; the transition plan has not been updated to list it]** Window is N and N-1 product minors. Alternative considered: same minor only (no shims, but every upgrade is a fleet-wide flag day).
- **[settled in brainstorming]** Raft has no separate version gate: `/raft/*` carries the same `X-Wayhouse-Protocol` header check, and changing a raft RPC payload type counts as a breaking wire change (protocol major bump).

## Open questions

- Whether the raft log and snapshot storage need their own on-disk version beyond the store marker; decide when the openraft dependency next changes.
