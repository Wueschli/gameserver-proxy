# Plugin API and tick runner under HA (Wave 5, slice 6)

**Goal:** Make the replicated plugin state of slice 5 reachable: `/plugins` on an HA tier and a tick runner that runs on the Raft leader only. Also folds in the hardening of #282.

**Spec:** `docs/superpowers/specs/2026-10-07-plugin-system-design.md` (HA section) and the automation hooks addendum. Builds on [slice 5](2026-10-08-plugin-replicated-state.md). Refs #220, closes #282.

## Decisions

- **Cursor (#282.1).** `PluginStore::apply_at(index, op)` applies one replicated write and the applied-index cursor in one multi-tree sled transaction; an index at or below the cursor is a `Replayed` (the state machine answers `Revision(None)`, like the other stores). Rejections advance the cursor too. The cursor travels in `PluginSnapshot`.
- **Atomic restore (#282.2).** `PluginStore::replace` swaps installs, state and cursor in one transaction. The other stores' `replace_stores` stay as they are: a crash mid-replace leaves `last_applied_log` unchanged, so openraft installs the snapshot again, and installing is a full replace. Documented in `docs/plugins.md`.
- **Encoding and size (#282.3).** State bytes are base64 in snapshots and in `PluginState` entries. A commit is bounded (1 MiB, 256 keys, 256-byte keys, the host's own limits) before it is proposed and again when applied.
- **Validation (#282.4).** `InstallRecord::validate` runs in the API before proposing and in apply (`PluginReject::Invalid`), so a malformed record from any proposer is refused the same way on every replica.
- **Revision (#282.5).** `PluginApplied(Option<u64>)` carries the new state revision of a state commit.
- **Snapshot consistency (#282.6).** `copy()` runs on the state-machine worker, serialised with `apply`, so the plugin snapshot is at the snapshot's `last_applied`. A comment says so.
- **API.** Reads local. Enable, disable and delete are proposed through `propose_write_as` (follower forwards). Upload, install and status run on the leader; a follower forwards the whole request with `forward_to_current_leader` (a forwarded request that lands on a non-leader answers 503 instead of bouncing).
- **Tick.** `Runner::new_ha`: only the leader ticks; a node that is not the leader drops its schedule, loaded plugins and statuses; a new term resets the schedule, which is the grace period. The commit is a `PluginState` entry tagged with the term the leader read.

## Out of scope

Module blob replication (slice 7): an install works against the leader that took the upload; a later leader without the bytes reports "this node does not hold the module bytes".

## Tasks (all done test-first)

1. Store: cursor, `apply_at`, validation, base64, atomic `replace` (`plugins.rs` tests).
2. State machine: route the four entries through `apply_at`, `PluginApplied(Option<u64>)`, `PluginReject::Invalid`.
3. API: leader routing, proposals, status forwarding (`plugins/api.rs` tests on a single-node Raft group).
4. Runner: leader gating, grace on a new term, Raft commit (`plugins/runner.rs` tests).
5. Fleet test `ha_plugins`: three controllers, upload and install via a follower, replication to every replica, status via a follower, failover to a new ticking leader.
6. Docs, HANDOVER, AGENTS.
