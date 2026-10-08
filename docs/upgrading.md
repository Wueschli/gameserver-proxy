# Upgrading a fleet

How to move a running deployment to a newer version one component at a time, without a
flag day. The design and its reasoning are in the
[component versioning spec](superpowers/specs/2026-10-05-component-versioning-design.md);
this page is the operator's view of what is built. Releases and tags are in
[`RELEASING.md`](../RELEASING.md).

## Compatibility rules

- **Window: N and N-1.** Two adjacent minor versions of the product (0.N and 0.(N-1)) are
  meant to run side by side during an upgrade. While the product is 0.x, "minor" is the
  second number.
- **Protocol major** must match. A node that receives another major (or an unreadable
  `X-Wayhouse-Protocol` header) answers `426` and counts it in
  `wayhouse_protocol_mismatch_total`. A breaking wire change bumps the major and, to keep
  the window honest, ships with a compatibility shim for the previous major for one
  product minor (no second major exists yet, so no shim exists yet).
- **Protocol minors are additive.** A sender only uses a newer optional field or route
  with a peer that has said it understands it: every request carries the caller's
  version and every response the server's, and a peer whose version is not known yet
  gets the baseline. Receivers ignore unknown JSON fields.
- **Config** is the exception to "controllers first": the config `schema_version` is the
  minimum schema a document needs, and the controller refuses (`422`) a document newer
  than the lowest `max_config_schema` any live proxy registration reported. Upgrade
  every proxy **before** you use a config field the new version introduced.
- **Store**: a controller refuses to open a store written by a newer build and migrates
  an older one once, on open.

What is and is not checked: the header check enforces protocol **majors only**. The
N / N-1 product window itself is a rule kept by review (see the `AGENTS.md` row for wire
changes) and exercised by tests that inject an older protocol minor into a real fleet
(`crates/wayhouse-fleet-tests/tests/mixed_versions.rs`). Once two releases exist, the
non-required `compat` workflow also runs the previous release's images against the
current ones.

## Version table

One row per release, newest first. A CI check
(`.github/scripts/check_upgrading_doc.py`) fails when the first row disagrees with the
constants in the source, so a protocol, schema, store or ABI change cannot land without
its row. Use `unreleased` for the product column until the release is tagged.

| Release    | Protocol | Config schema | Store format | Sniffer ABI |
| ---------- | -------- | ------------- | ------------ | ----------- |
| unreleased | 1.1      | 1             | 1            | 0.1         |
| 0.1.0      | 1.0      | 1             | 1            | 0.1         |

## Order of operations

1. **Aggregator and UI** (the read side).
2. **Controller tiers**, leaf `slave` tiers first and the root tier last. A `slave` tier
   is a child controller under a parent. Inside the window either order works because
   of the gating rule above; going leaves-first keeps the root, which owns writes, on the
   version your operators have run longest.
3. **Raft HA inside one tier**, see below. A tier made of several controllers is a
   different mechanism from the parent and child tiers.
4. **Proxies and agents**, a few at a time: drain, upgrade, rejoin.
5. **Config fields** introduced by the new version, only after every proxy runs it.

Check the fleet view in the UI between steps: an instance more than one minor behind the
newest, on another protocol major, or that refused a request for an incompatible protocol
in the last five minutes is red; one minor behind is yellow.

## Controller tiers

Upgrade a tier completely (all its replicas, see below) before moving to its parent.
Parent-to-child and child-to-parent calls both carry protocol versions, so a child one
minor older than its parent is sent only baseline fields. A `slave` tier relays writes
upward through its leader; a tier mid-upgrade keeps relaying as long as the protocol
major matches.

## Raft HA inside a tier

Replicas of one tier replicate through Raft; `/raft/*` carries the same protocol header
check, and changing a raft RPC payload counts as a breaking wire change.

1. Upgrade the **followers** one at a time. Wait for the upgraded node to catch up
   (`GET /admin/ha/members` shows it as a voter again) before touching the next.
2. There is no leader-transfer command. To move leadership, stop the leader process; the
   followers elect a new leader within the election timeout and writes resume through it.
3. Upgrade the former leader last.

This sequence is **not covered by an automated test** (the fleet tests do not run a
rolling Raft upgrade); treat the first use in your environment as a rehearsal. A write
that a follower forwards to the leader is sent with the follower's protocol version, not
the original caller's; that is harmless inside the window because receivers ignore
fields they do not know.

## Proxies

A proxy upgrade relies on draining:

1. `POST /admin/drain`: the instance reports not ready, so the load balancer or anycast
   routes new clients elsewhere. The data path keeps running.
2. Wait for sessions to end (`active_conns` in `GET /config`, or let
   `shutdown_grace_sec` expire).
3. Stop the old process, start the new one, check `GET /readyz`, and let the load
   balancer bring it back.

A config reload that replaces a UDP listener no longer drops new flows while the old
group drains (#186): the old workers leave the kernel's `SO_REUSEPORT` hash at once, keep
answering their live sessions, and a client of such a session gets a new session in the
replacement group on its next datagram.

## Agents

Agents register with the controller like proxies do. Upgrade them after the controller
tier they register with; an agent one minor older than its controller works against
baseline fields.

## Kubernetes

The same order applies, with a rolling update (`maxUnavailable: 1`) and the readiness
probe gating each step. Pending [#88](https://github.com/wayhouse-proxy/wayhouse/issues/88):
the packaged manifests for this do not exist yet, so this section describes the intent,
not a tested procedure.

## Rollback

Roll back in the reverse order. A downgrade is safe inside the window for the protocol.
A controller store that a newer build has migrated cannot be opened by an older build
(`FormatTooNew`): restore the store from a backup taken before the upgrade, or keep the
upgraded controller. Config documents that use a field of the newer schema are rejected
by older nodes, so revert the controller to the previous config revision first.

## Troubleshooting

| You see                                                                                                 | It means                                                               | What to do                                                                                                         |
| ------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------ |
| `426` with `wayhouse protocol <peer> is not compatible with this node (<ours>): upgrade the older side` | The two sides have different protocol majors.                          | Upgrade the older side; a log line with the same text and `wayhouse_protocol_mismatch_total` name the route group. |
| `config schema_version N is newer than this build supports (max M)`                                     | A document needs a newer schema than this node knows.                  | Upgrade the node, or submit a document that declares a lower `schema_version` and does not use the newer fields.   |
| `` `path` needs schema_version N or higher ``                                                           | The document uses a field newer than the `schema_version` it declares. | Set `schema_version: N` once every proxy supports it.                                                              |
| Controller refuses a config with `422` naming a proxy's maximum                                         | A live proxy registration reports a lower `max_config_schema`.         | Upgrade that proxy first.                                                                                          |
| `store <path> has format N, newer than this build supports` (`FormatTooNew`)                            | An older controller was pointed at a store a newer build migrated.     | Run the newer build, or restore the pre-upgrade store backup.                                                      |
