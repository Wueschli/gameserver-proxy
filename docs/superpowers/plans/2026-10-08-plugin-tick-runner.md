# Plugin tick runner and plugin state (Wave 5, slice 4)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** On a standalone controller started with `--plugins`, run every enabled plugin that declared `on_timer` at its approved tick interval, persist the plugin's state, and show the operator what it did (last result, recent log lines).

**Architecture:** A `Runner` task in `wayhouse-controller::plugins::runner` scans the installs once a second. A plugin is compiled on first use through the `CompilePool` (this is what closes #265), `init` runs once with the install's config, then `on_timer` runs on the pool at each due time. Every call returns `Effects`; the runner commits the `state_puts` as one batch that is compare-and-set on a per-install state revision (the single-node form of the term-and-revision commit in the design), then records status and log lines in memory. A single-node controller is always the leader, so the HA term check and replication are the next slice; HA and slave controllers keep answering 501.

**Tech Stack:** Rust, tokio, sled, `wayhouse-plugin-host`.

**Spec:** `docs/superpowers/specs/2026-10-07-plugin-system-design.md` (HA and ticks) and the automation hooks addendum (tick result committed as one entry; new leader waits before its first tick). Builds on slices [1](2026-10-08-plugin-host-foundation.md), [2](2026-10-08-plugin-compile-bounds-and-check.md), [3](2026-10-08-plugin-install-store-and-api.md). Refs #220; closes #265.

## Global Constraints

- A plugin never runs on the request path or the runtime threads: loading and calls go through the bounded `CompilePool` (a full queue skips that tick, it never blocks).
- At most one call per install in flight; a plugin that did not declare `on_timer` is never ticked.
- First tick one full interval after the runner starts or the install is enabled (the grace period of the design).
- State writes are applied only through the revision check; a stale commit is dropped, not retried.
- No secrets, no `http`, no routes in this slice: a plugin can still only log and keep state.
- Status and logs are in memory only; they are not replicated and are lost on restart.

## Review Focus

- A stale revision (state changed between the call's snapshot and its commit, for example by a delete and re-create) commits nothing.
- Disabling or deleting an install stops its ticks; deleting removes its state.
- A trapping or timed-out plugin records an error, keeps its previous state, and keeps ticking at its interval (no hot loop).
- A full pool queue skips the tick and records it.
- Enabling a plugin never runs it in the request; the first tick is one interval later.

---

### Task 1: Plugin state in the store

**Files:** Modify `crates/wayhouse-controller/src/plugins.rs`.

**Interfaces:**
- Produces: `PluginStore::state(&self, id) -> Result<(u64, StateSnapshot), PluginStoreError>`; `commit_state(&self, id, expected_rev: u64, puts: &BTreeMap<String, Vec<u8>>) -> Result<u64, CommitError>` where `CommitError::{Stale, NoSuchInstall, Store(PluginStoreError)}`; `delete` also removes the install's state.

- [ ] **Step 1:** tests: fresh state is `(0, {})`; a commit with the right revision applies all puts and returns rev+1; a commit with an old revision returns `Stale` and changes nothing; commit for an unknown install returns `NoSuchInstall`; `delete` removes state; two installs' states do not mix.
- [ ] **Step 2:** run, FAIL. **Step 3:** implement (one sled tree, keys `id\0k:<key>` and `id\0rev`, applied as one batch under the store's write lock). **Step 4:** PASS. **Step 5:** commit `feat(plugin): persist plugin state with a revision check`.

### Task 2: Public pool call entry

**Files:** Modify `crates/wayhouse-plugin-host/src/pool.rs`.

- [ ] **Step 1:** test that a public `CompilePool::call` runs a closure on a worker and returns `Busy` on a full queue (reuse the existing gate-channel test pattern). **Step 2:** FAIL. **Step 3:** make `run` public as `call` with the pool's timeout. **Step 4:** PASS. **Step 5:** commit `feat(plugin): let the pool run guest calls`.

### Task 3: The runner

**Files:** Create `crates/wayhouse-controller/src/plugins/runner.rs`.

**Interfaces:**
- Produces: `Runner::new(store, pool)`; `Runner::run_due(&self, now: Instant)` (one scheduling pass, awaits the calls it started); `Runner::spawn(self: Arc<Self>) -> JoinHandle<()>` (1 s loop); `Runner::status(&self, id) -> Option<PluginStatus>` with `{last_tick_unix, last_ok: Option<bool>, last_error: Option<String>, consecutive_failures, ticks, logs: Vec<(level, msg)>}` (last 100 lines).
- Consumes: Task 1 and 2.

- [ ] **Step 1:** tests with WAT fixtures and a fake clock passed to `run_due`: a counter plugin ticks once per interval and its state survives; first tick is one interval after first sight; not due before the interval; disabled install is not ticked; a trapping plugin records the error, keeps old state and is ticked again next interval; a plugin without `on_timer` is never ticked; log lines appear in status; a commit made stale by a concurrent state change is dropped; a busy pool skips the tick and says so.
- [ ] **Step 2:** run, FAIL. **Step 3:** implement: load cache keyed by install id and sha256, `init` once per load (its effects committed the same way), per-install `next_due`, one in-flight call per install. **Step 4:** PASS. **Step 5:** commit `feat(plugin): run enabled plugins on a timer`.

### Task 4: API status, wiring, docs

**Files:** Modify `plugins/api.rs` (`GET /plugins/{id}/status`), `main.rs` (spawn the runner when plugins are available), `docs/plugins.md`, `AGENTS.md`, `HANDOVER.md`.

- [ ] **Step 1:** API test: status of an unknown install is 404; of a never-ticked install is an empty status. **Step 2:** FAIL, implement, PASS. **Step 3:** docs: what ticks do, the grace period, state revision rule, what is not built (HA, secrets, http, routes). **Step 4:** `make check`. **Step 5:** commit `feat(plugin): tick enabled plugins on the standalone controller`.
