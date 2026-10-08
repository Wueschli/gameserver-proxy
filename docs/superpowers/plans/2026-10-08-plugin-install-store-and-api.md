# Plugin install store and admin API (Wave 5, slice 3) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let an operator upload a plugin module to a standalone controller, see the capabilities it declares, approve them, and manage the resulting installs (list, enable, disable, delete) over the admin API. Nothing runs the plugin yet.

**Architecture:** A `plugins` module in `wayhouse-controller` keeps install records and content-addressed module blobs in two sibling `sled` trees of the controller's database. Installing compiles the module through `wayhouse_plugin_host::CompilePool` with the operator's approved capabilities, so a record only exists for a module the host would load. The routes sit behind the same admin bearer layer as `/config`. The feature is opt-in (`--plugins`) and standalone-only; HA and slave controllers answer 501 until the replicated slice.

**Tech Stack:** Rust, axum, sled, `wayhouse-plugin-host`.

**Spec:** `docs/superpowers/specs/2026-10-07-plugin-system-design.md` (install flow, trust, install identity) and the automation hooks addendum ("Module bytes in HA" is the later slice; this one is standalone). Builds on slices [1](2026-10-08-plugin-host-foundation.md) and [2](2026-10-08-plugin-compile-bounds-and-check.md). Refs #220, #265.

## Global Constraints

- Approval is recorded against the module sha256 plus the approved capability set, in the install record.
- Module upload limit: 8 MiB (`wayhouse_plugin_host::MAX_MODULE_BYTES`).
- Install identity: a random install id, not the plugin name.
- Admin-authenticated like every other mutating route; no new auth scheme.
- The module is compiled through the `CompilePool`, never on the request task.
- No secrets, no ticks, no HA in this slice.

## Review Focus

- Installing with approved capabilities narrower than the module declares is refused with 422 and stores nothing.
- Uploading a module twice stores one blob; deleting one of two installs of the same module keeps the blob, deleting the last removes it.
- A request body over 8 MiB is refused before it is read in full.
- A full compile queue answers 503, not a hang.
- `--plugins` off (default), HA, or `--role slave`: the `/plugins` routes answer 501 with a reason.

---

### Task 1: `PluginStore` and records

**Files:** Create `crates/wayhouse-controller/src/plugins.rs`; modify `lib.rs`, `Cargo.toml`; add `Serialize` to `Capabilities`, `Triggers`, `StateCap` in `wayhouse-plugin-host/src/caps.rs`.

**Interfaces:**
- Produces: `pub struct InstallRecord { id, name, sha256, size, approved: Capabilities, config: serde_json::Value, enabled: bool, created_at: u64, created_by: Option<String> }`; `PluginStore::open(&sled::Db) -> Result<Self, sled::Error>`; `put_blob(&self, sha256: &str, bytes: &[u8])`; `get_blob(&self, sha256) -> Result<Option<Vec<u8>>, _>`; `create(&self, InstallRecord)`; `list() -> Vec<InstallRecord>`; `get(id) -> Option<InstallRecord>`; `set_enabled(id, bool) -> Option<InstallRecord>`; `delete(id) -> bool` (removes the blob when no install references it).

- [ ] **Step 1:** tests in `plugins.rs`: create/get/list round trip; `set_enabled`; blob stored once for two installs of one sha; delete keeps the blob while another install references it and removes it with the last.
- [ ] **Step 2:** run, FAIL. **Step 3:** implement. **Step 4:** PASS. **Step 5:** commit `feat(plugin): store plugin installs and module blobs in the controller`.

### Task 2: Admin API

**Files:** Create `crates/wayhouse-controller/src/plugins/api.rs`.

**Interfaces:**
- Consumes: Task 1's store, `CompilePool`.
- Produces: `PluginsState { store: PluginStore, pool: Arc<CompilePool>, auth_token: Option<Arc<str>> }`, `pub fn router(PluginsState) -> Router` with `POST /plugins/modules` (raw body; returns `{sha256, size, abi, capabilities}`), `POST /plugins` (`{name, sha256, approved, config?, enabled?}` returns the record, 201), `GET /plugins`, `GET /plugins/{id}`, `POST /plugins/{id}/enable`, `POST /plugins/{id}/disable`, `DELETE /plugins/{id}` (204); `pub fn disabled_router(reason: &'static str) -> Router` answering 501 on `/plugins` and `/plugins/{*rest}`.

- [ ] **Step 1:** tests with `tower::ServiceExt::oneshot` and WAT fixtures for the Review Focus cases plus: bad wasm 400, unknown sha 404, name rules 400, bearer required when a token is set.
- [ ] **Step 2:** run, FAIL. **Step 3:** implement; map `PoolError::Busy`/`TimedOut` to 503, `ModuleError` to 422.
- [ ] **Step 4:** PASS. **Step 5:** commit `feat(plugin): controller admin API for plugin installs`.

### Task 3: Wiring and docs

**Files:** Modify `crates/wayhouse-controller/src/main.rs` (`--plugins` flag, merge routers), `docs/plugins.md`, `AGENTS.md`, `HANDOVER.md`.

- [ ] **Step 1:** a `wayhouse-fleet-tests`-free smoke check: controller starts with and without `--plugins` (unit test of the flag decision function).
- [ ] **Step 2:** write docs (API table, trust and approval flow, what is not built). **Step 3:** `make check`. **Step 4:** commit `feat(plugin): opt-in plugin routes on the standalone controller`.
