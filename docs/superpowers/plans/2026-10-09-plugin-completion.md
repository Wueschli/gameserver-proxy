# Finishing the plugin system in one PR (Wave 5, slices 7 to 9)

**Goal:** Finish the plugin build (#220) apart from registry install: module replication under HA, secret storage with the `http` capability, routes, webhooks and events. One PR with one commit per part, each `make check` green before the next starts, to save review, conflict and CodeRabbit rounds.

**Specs:** [plugin system](../specs/2026-10-07-plugin-system-design.md), [automation hooks addendum](../specs/2026-10-07-plugin-automation-hooks-design.md), [secret storage](../specs/2026-10-08-plugin-secret-storage-design.md). Builds on [slice 6](2026-10-09-plugin-ha-api-and-leader-tick.md).

## Part 1: module replication (closes #285, #286; part of #284)

- **Push to a quorum, then record (addendum).** The leader `PUT`s the module to every member over `/raft/plugin-blob/{sha256}` (peer token, protocol gate, hash checked on arrival) and proposes the install only once a majority of voters holds it. A quorum that cannot be reached is a `503`.
- **Catch-up fetch.** Every replica runs a loop that fetches the modules its installs reference and it lacks, leader first, verifying the hash. A leader that finds none raises a repeated warning.
- **Rolling-upgrade gate (#284.1).** `/raft/whoami` reports `plugin_support`; a new install is refused while any member reports less than 2 or does not answer. Existing installs are not gated.
- **Leadership lost mid-install (#284.2).** The install proposal is made on the leader and not forwarded; a lost leadership is `503 retry`.
- **Orphan sweep (#284.3).** Startup removes blobs no install references.
- **Not done from #284:** point 4 (ignore `x-wayhouse-forwarded` unless from a known peer address, no security impact) and point 5 (the early name check in the API avoids a compile for a bad name; kept on purpose).
- **#286.** The busy-pool test offers its holding job until the worker takes it (a pool with no queue accepts a job only while a worker waits), instead of racing the worker's start.

## Tasks (test-first)

1. Store: `sweep_blobs`, `missing_blobs`.
2. `plugins/peer.rs`: routes, quorum push, support gate, catch-up fetch, loop.
3. API install path; `main.rs` wiring; runner message.
4. Fleet test `ha_plugins`: the new leader keeps ticking after failover.
5. Docs, HANDOVER, AGENTS.
