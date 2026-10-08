# Replicated plugin state machine (Wave 5, slice 5)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make plugin install records and plugin state part of the replicated controller state, so every replica of an HA tier holds the same installs and the same per-install state, in the log and in snapshots. Nothing in this slice is reachable by an operator yet: the `/plugins` API and the tick runner still answer 501 and do nothing on an HA controller. This is the state-machine layer the next two slices build on.

**Architecture:** Four new `WriteRequest` entries (`PluginInstall`, `PluginSetEnabled`, `PluginDelete`, `PluginState`) applied by `StateMachineStore::apply` into the same `PluginStore` the standalone controller uses (two more trees in the same sled database, opened by the state machine on every HA replica so all replicas apply the same entries whatever their flags). `PluginState` carries the proposing leader's term and the state revision its call read, and is applied compare-and-set: it commits only if the entry's own term equals the tagged term and the install's state revision is unchanged. Snapshots gain a `plugins` section (install records, state and revisions; module blobs stay out of the log and snapshots, per the design).

**Tech Stack:** Rust, openraft 0.9, sled.

**Spec:** `docs/superpowers/specs/2026-10-07-plugin-system-design.md` (HA section: one replicated entry per tick, tagged with the leader term and the revision read) and the automation hooks addendum ("Module bytes in HA": the log carries only the install record). Builds on slices [3](2026-10-08-plugin-install-store-and-api.md) and [4](2026-10-08-plugin-tick-runner.md). Refs #220.

## Decomposition of the HA work (decided here)

1. **This slice (5):** replicated state machine layer, unreachable.
2. **Slice 6:** serve `/plugins` on HA controllers (writes proposed through `propose_write`, followers forward to the leader) and run the tick runner on the leader only, with the grace period after an election and the term-tagged commit; installs allowed only when the leader holds the module blob.
3. **Slice 7:** module blob replication (peer fetch endpoint with peer auth, quorum acknowledgement before the install record is committed, "pending" status on a node without the blob, alert on a leader without a blob it needs).

## Global Constraints

- Every replica applies every plugin entry to the same state, whether or not it runs `--plugins`; the flag only controls the API and the runner.
- Applying is deterministic and a refusal is a normal response (`PluginRejected`), never a storage error, so one bad entry cannot wedge the state machine.
- The module blob is neither in the log nor in a snapshot.
- Rolling upgrades: the component-versioning gating (X-Wayhouse-Protocol) is not implemented in the controller yet. Plugin entries are proposed only by the slice-6 API behind `--plugins`, and a cluster that uses plugins must run one build on all replicas. This is stated in `docs/plugins.md`; the gate itself is the versioning work.
- An old-format snapshot without a `plugins` section installs as "no plugins" (`#[serde(default)]`).

## Review Focus

- Replaying a log (two replicas applying the same entries) gives identical install records and state, including after a snapshot install.
- A stale `PluginState` (revision moved, or tagged term not the entry's term) commits nothing and answers `PluginRejected`.
- `PluginDelete` removes the install's state; a snapshot taken after it holds neither.
- A duplicate `PluginInstall` id is refused, not overwritten.
- Re-applying the same committed entry after a crash does not corrupt state (idempotent apply).

---

### Task 1: Store operations for replicated apply

**Files:** Modify `crates/wayhouse-controller/src/plugins.rs`.

**Interfaces:**
- Produces: `PluginStore::apply_install(&InstallRecord) -> Result<ApplyOutcome, PluginStoreError>` (`Applied`, `Exists`; no blob check, since the blob is out of band; an identical record already stored counts as `Applied` so a crash replay is harmless); `snapshot() -> PluginSnapshot { installs: Vec<InstallRecord>, state: Vec<StateSnap { id, rev, entries }> }`; `replace(&PluginSnapshot)` (one batch per tree; blobs untouched).
- `commit_state`, `set_enabled`, `delete` already exist and are reused by apply.

- [ ] **Step 1:** tests: `apply_install` stores without a blob; a second apply of an identical record is `Applied`, of a different record with the same id `Exists`; `snapshot` then `replace` into a second store reproduces installs, state and revisions exactly; `replace` removes installs and state not in the snapshot; blobs are left alone.
- [ ] **Step 2:** FAIL. **Step 3:** implement. **Step 4:** PASS. **Step 5:** commit `feat(plugin): store operations for replicated plugin entries`.

### Task 2: Entries and apply

**Files:** Modify `crates/wayhouse-controller/src/ha/mod.rs` (variants, `PluginReject`), `ha/state_machine.rs` (store field, apply arms).

**Interfaces:**
- Produces: `WriteRequest::{PluginInstall(InstallRecord), PluginSetEnabled { id, enabled }, PluginDelete { id }, PluginState { id, expected_rev, term, puts }}`; `WriteResponse::{PluginApplied(Option<u64>), PluginRejected(PluginReject)}` with `PluginReject::{Exists, NoSuchInstall, Stale, WrongTerm}`.

- [ ] **Step 1:** state-machine tests (using the existing test harness in `state_machine.rs`): install then state commit then read; a second install with the same id is refused; set_enabled and delete; a state entry with a stale revision is `Stale`; one whose `term` differs from the entry's log term is `WrongTerm`; delete removes state; unknown id answers `NoSuchInstall`; an entry for an install deleted meanwhile answers `NoSuchInstall`.
- [ ] **Step 2:** FAIL. **Step 3:** implement. **Step 4:** PASS. **Step 5:** commit `feat(plugin): apply plugin entries in the HA state machine`.

### Task 3: Snapshots

**Files:** Modify `ha/state_machine.rs` (`SnapshotContent.plugins`, `copy`, `replace_stores`).

- [ ] **Step 1:** tests: a snapshot built after installs and state commits, installed into a fresh state machine, gives the same `PluginStore` contents; a snapshot JSON without a `plugins` key decodes to empty and installs as no plugins; installing a snapshot removes plugin state not in it.
- [ ] **Step 2:** FAIL. **Step 3:** implement. **Step 4:** PASS. **Step 5:** commit `feat(plugin): carry plugin installs and state in HA snapshots`.

### Task 4: Two-replica convergence, docs, check

**Files:** Modify `ha/state_machine.rs` tests (or the existing HA test module), `docs/plugins.md`, `AGENTS.md`, `HANDOVER.md`, transition index.

- [ ] **Step 1:** test: two state machines applying the same entry sequence (installs, state commits, enable, delete) end with identical `PluginStore::snapshot()`; one then catches up via snapshot install and matches.
- [ ] **Step 2:** docs: the HA decomposition, the one-build rule, what is not reachable yet. **Step 3:** `make check`. **Step 4:** commit `docs(plugin): record the replicated plugin state layer`.
