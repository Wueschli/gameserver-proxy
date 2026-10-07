# Sniffer Updates and Rollback Implementation Plan (#184)

> **Terminology update (2026-10-07).** Leandro split the old "plugin" concept in two. **Sniffers** are the WASM protocol/hostname sniffer modules and live in [`wayhouse-proxy/sniffers`](https://github.com/wayhouse-proxy/sniffers). **Plugins** are integrations with other systems (e.g. the Pelican panel, #213) and live in [`wayhouse-proxy/plugins`](https://github.com/wayhouse-proxy/plugins); their design is still open. Wherever this document says "plugin" or "plugins repo" for a WASM sniffer module, read **sniffer** / **sniffers repo**. Code identifiers, crates and paths were renamed to "sniffer" in the same PR as this banner; older text below may still show the old names.


> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans (project convention: no subagents in implementation threads). Steps use checkbox (`- [ ]`) syntax.

**Goal:** From the Sniffers page an operator checks on demand whether installed sniffers have newer compatible versions, updates them across the fleet, and can roll back, with the proxy falling back to the previous module automatically when a new file fails validation.

**Architecture:** Proxy side: the upload handler keeps the replaced module as `.<name>.wasm.prev` (dotfile, ignored by the `*.wasm` scan), a rollback endpoint swaps it back, and `scan` prefers `.prev` when the current file fails validation. UI backend side: an "check updates" route maps each instance's installed `sha256` to versions in the loaded registry indexes and reports updates; the update itself is the install flow from the previous plan with the target version.

**Tech Stack:** Rust (wayhouse admin and loader, wayhouse-ui), React.

**Spec:** `docs/superpowers/specs/2026-10-05-plugin-registry-design.md` ("Updates"). Prerequisites merged: `2026-10-05-plugin-install-and-trust.md`, `2026-10-05-sniffer-upload-validation.md`.

## Global Constraints

- Discovery is **on demand only**: no timers, no background fetches (decided).
- Pinned-only by default: an update never changes a pin; instances with pins get the pin snippet as in the install plan.
- `.prev` names: `.<name>.wasm.prev`; exactly one previous version is kept; the dotfile must be skipped by `list_sniffers` and `scan` (both filter on the `.wasm` extension, and `.prev` has a different extension, test it anyway).
- Pins: an update upload on a pinned instance is refused by the proxy (`409 pinned`, nothing written; see the upload-validation plan). Rollback with pins set swaps `.prev` back **only if its sha256 equals the pin**, else `409` and nothing changes; the automatic fallback likewise loads `.prev` only when it matches the pin. A rolled-back or replaced file must never leave a state that `scan` would reject, because that makes the next startup fatal.
- New admin route is admin-authenticated like the others; fleet fan-out route in the aggregator and UI proxy mirrors `DELETE /fleet/sniffers/{name}`.
- `make check`, `make ui-test` pass.

## Review Focus

- Update when `.prev` already exists: the old `.prev` is replaced, never both kept; two updates in a row followed by rollback returns the version before the last update only.
- Rollback with no `.prev` is a clear `404`-style error, not a silent success.
- A module installed by hand (hash in no index) shows "unknown build" and offers no update, rather than guessing.
- Automatic fallback is logged at error level once per rescan with both names, and the instance listing shows it (`loaded: true`, plus `fallback: true`).
- In-flight sniffs during the swap finish on the old instance (existing property of per-call `Store`; add one test that proves a call spanning a rescan completes).

---

### Task 1: Keep the previous module and roll back (proxy admin)

**Files:**
- Modify: `crates/wayhouse/src/admin.rs` (`upload_sniffer`, new `rollback_sniffer`, route `POST /admin/sniffers/{name}/rollback`, `SnifferInfo` gains `has_previous: bool`)
- Test: `admin.rs` `mod tests`

**Interfaces:**
- Produces: `POST /admin/sniffers/{name}/rollback` -> `200` plain text `rolled back <name>`; `404` when there is no previous; reload requested after a swap. `SnifferInfo.has_previous`.

- [ ] **Step 1: Write failing tests**: `upload_over_existing_keeps_previous`, `second_upload_replaces_previous_not_accumulates`, `rollback_restores_previous_and_requests_reload`, `rollback_without_previous_is_404`, `rollback_on_pinned_instance_with_nonmatching_previous_is_409_and_changes_nothing`, `list_hides_dotfiles_and_reports_has_previous`, `delete_removes_previous_too`.
- [ ] **Step 2: Run** `cargo test -p wayhouse admin::tests::rollback admin::tests::upload`. Expected: FAIL.
- [ ] **Step 3: Implement**: in upload, before the atomic rename, `rename(<name>.wasm -> .<name>.wasm.prev)` when a current file exists (copy semantics if the cross-step must be crash-safe: write new file to tmp first, then `rename(current, prev)`, then `rename(tmp, current)`; document the tiny window and that a crash in it leaves `.prev` plus tmp, recoverable by the scan fallback below). Rollback swaps with a temp name.
- [ ] **Step 4: Run** the same command. Expected: PASS. **Commit** `feat: sniffer upload keeps the previous module, rollback endpoint (#184)`.

### Task 2: Automatic fallback in `scan`

**Files:**
- Modify: `crates/wayhouse/src/sniffer_loader.rs` (`scan`)

**Interfaces:**
- Consumes: `validate` (sniffer-validation plan), pin check logic in `scan`.
- Produces: `scan` loads `.<name>.wasm.prev` when `<name>.wasm` fails validation (and, with pins, only if the previous hash matches the pin); the loaded map entry is under `name`; new `pub fn fallbacks(&self) -> Vec<String>` is not needed: expose through `WasmSniffer.fallback: bool` set on the instance and read by `list_sniffers` via the registry (extend `Sniffers::names()` consumer in `admin.rs` with a `fallback_names()` accessor on the `Sniffer` trait default `false`; keep the trait change minimal).

- [ ] **Step 1: Write failing tests**: `scan_falls_back_to_previous_when_current_is_invalid`, `scan_without_previous_skips_as_before`, `fallback_respects_pins_hash_mismatch_means_no_fallback`, `fallback_instance_reports_fallback_true`, `call_spanning_a_rescan_completes_on_the_old_instance`.
- [ ] **Step 2: Run** `cargo test -p wayhouse sniffer_loader::tests::fallback`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test -p wayhouse` and `cargo test -p wayhouse --no-default-features`. **Commit** `feat: scan falls back to the previous module when the new file is invalid (#184)`.

### Task 3: Fleet fan-out for rollback (aggregator and UI proxy)

**Files:**
- Modify: `crates/wayhouse-aggregator/src/api.rs` (+ fan-out), `crates/wayhouse-ui/src/aggregator_proxy.rs`, `proxy_util.rs` (allow-list test table lines 48 to 57 shows the pattern)

**Interfaces:**
- Produces: `POST /fleet/sniffers/{name}/rollback` (aggregator) and `POST /api/fleet/sniffers/{name}/rollback` (UI), same per-instance result shape as upload.

- [ ] **Step 1: Write failing tests** mirroring the existing delete fan-out tests: `fleet_rollback_fans_out_to_every_instance`, `fleet_rollback_reports_partial_failures`, `ui_proxies_rollback_with_the_actor_header_and_mutating_role`.
- [ ] **Step 2: Run** `cargo test -p wayhouse-aggregator rollback && cargo test -p wayhouse-ui rollback`. Expected: FAIL.
- [ ] **Step 3: Implement** by copying the delete route's shape (do not abstract unless the third copy appears).
- [ ] **Step 4: Run.** **Commit** `feat: fleet-wide sniffer rollback (#184)`.

### Task 4: Update discovery on demand (UI backend)

**Files:**
- Modify: `crates/wayhouse-ui/src/registry_api.rs`
- Test: same file

**Interfaces:**
- Consumes: `RegistryClient::fetch_index` (with `invalidate` so the button always refetches), per-instance sniffer listings (`GET /api/fleet/instances/{instance}/sniffers`).
- Produces: `POST /api/registries/updates/check` (POST because it refetches; no body) -> `[{plugin, installed_sha256, installed_version?: string, known: bool, update?: {registry_id, version, compatible: bool, reason?: string}, instances:[string]}]`; `POST /api/registries/{id}/install {name, version}` is reused for the update itself (Task 3 of the previous plan).

- [ ] **Step 1: Write failing tests**: `check_finds_newer_compatible_version`, `check_marks_unknown_build_without_update`, `check_ignores_incompatible_newer_version_but_reports_reason`, `check_groups_instances_by_installed_hash` (a half-upgraded fleet shows two rows for one sniffer), `check_refetches_even_when_cached`, `check_with_unreachable_registry_reports_it_and_still_answers_for_the_rest`.
- [ ] **Step 2: Run** `cargo test -p wayhouse-ui updates`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run.** **Commit** `feat(ui): on-demand plugin update check (#184)`.

### Task 5: Sniffers page: update and rollback

**Files:**
- Modify: `PluginsPage.tsx`, `PluginsPage.test.tsx`, `api.ts`, `types.ts`

**Interfaces:**
- Consumes: Task 3 and 4 routes.
- Produces: a "Check for updates" button (no polling), per-plugin "Update to X" with the same limits and risk dialog as install, per-plugin "Roll back" when any instance reports `has_previous`, a visible "fallback active" badge.

- [ ] **Step 1: Write failing tests**: `check_button_calls_the_check_route_once_and_does_not_poll`, `update_row_shows_version_and_opens_dialog`, `rollback_button_only_when_has_previous`, `fallback_badge_is_shown`, `unknown_build_has_no_update_button`.
- [ ] **Step 2: Run** `make ui-test`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `make ui-test`, `make ui`. **Commit** `feat(ui): update and roll back plugins from the Plugins page (#184)`.

### Task 6: Docs

**Files:**
- Modify: `docs/sniffers.md` (update and rollback section), `docs/06-operations-observability.md` if a metric was added (no), `HANDOVER.md`

- [ ] **Step 1: Document** the flow, `.prev` semantics, automatic fallback, the pinned-instance rule and what "unknown build" means. **Commit** `docs: plugin updates and rollback (#184)`.

---

## Self-review

Spec coverage: on-demand discovery, compatibility check (reuses `select`), download-verify-validate-swap (install flow), rollback manual and automatic, hot reload, per-node pull fleet rollout. Controller-driven staged rollout is explicitly deferred in the spec.
