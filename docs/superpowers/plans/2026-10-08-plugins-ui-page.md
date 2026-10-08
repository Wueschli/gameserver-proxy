# Plugins page in the web UI (Wave 5, slice 4) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let an operator upload a plugin module, read what it asks for, approve it, and enable, disable or delete installs from the web UI.

**Architecture:** `wayhouse-ui`'s backend proxies the controller's `/plugins` API (slice 3, #269) the same way it proxies `/config`: stateless, the browser session never becomes a bearer token. Reads are `Role::Viewer`, every write is `Role::Admin`. A new React page lists installs and walks upload, capability review, approval and install.

**Tech Stack:** Rust (axum, reqwest), React, Vitest.

**Spec:** `docs/superpowers/specs/2026-10-07-plugin-system-design.md` ("Capabilities and trust"); builds on [slice 3](2026-10-08-plugin-install-store-and-api.md). Refs #220, #207.

## Global Constraints

- Approval is recorded against the module sha256 plus the approved capability set: the UI approves exactly the set the upload response declared, never an edited one.
- Module upload limit: 8 MiB (`wayhouse_http::MAX_SNIFFER_MODULE_BYTES` has the same value).
- Uploads and installs are `Role::Admin`; the list is `Role::Viewer`. The UI never holds the controller token.
- Registry install, secrets and `http`/`routes` capabilities are out of scope.

## Review Focus

- A controller that answers 501 (plugins off, HA, slave) shows its reason instead of an empty list.
- An install id from the URL cannot steer the proxy to another controller path.
- A body over 8 MiB is refused by the UI before it reaches the controller.
- A non-JSON or error reply from the controller reaches the page as a readable message.
- The Install button stays disabled until the operator ticks the approval box.

---

### Task 1: Backend proxy routes

**Files:** Modify `crates/wayhouse-ui/src/controller_proxy.rs`.

**Interfaces:** Produces `GET /api/plugins` (viewer); admin: `POST /api/plugins/modules` (raw body, 8 MiB limit), `POST /api/plugins` (JSON), `POST /api/plugins/{id}/enable`, `POST /api/plugins/{id}/disable`, `DELETE /api/plugins/{id}`. JSON bodies keep `content-type: application/json` toward the controller.

- [ ] **Step 1:** tests in `controller_proxy.rs`: list forwards with the token; module upload forwards raw bytes and refuses 8 MiB + 1 with 413; install forwards the JSON body and its content type; enable/disable hit the right paths; delete of `../config` cannot leave `/plugins/`; the 501 reply passes through; an Operator session gets 403 on install.
- [ ] **Step 2:** run `cargo test -p wayhouse-ui plugins`, expect FAIL.
- [ ] **Step 3:** implement; factor the percent-encoding in `release` into `fn path_segment(name: &str) -> Option<String>` and let `proxy` take an optional content type.
- [ ] **Step 4:** PASS. **Step 5:** commit.

### Task 2: Page and API client

**Files:** Create `web/src/pages/PluginsPage.tsx`, `PluginsPage.test.tsx`; modify `web/src/api.ts`, `types.ts`, `App.tsx`, `components/Layout.tsx`.

**Interfaces:** `uploadPluginModule(bytes): Promise<PluginModule>`, `installPlugin({name, sha256, approved, enabled}): Promise<PluginInstall>`, `listPlugins()`, `setPluginEnabled(id, enabled)`, `deletePlugin(id)`.

- [ ] **Step 1:** page tests: lists installs with capability words; shows the 501 reason; upload shows declared capabilities in plain words and keeps Install disabled until approval is ticked; install sends exactly the declared set; enable/disable call the API; delete asks first.
- [ ] **Step 2:** `npm test` FAIL. **Step 3:** implement. **Step 4:** PASS. **Step 5:** commit.

### Task 3: Docs, advisory bump, check

- [ ] **Step 1:** `npm update source-map-js` (lockfile only, #207), `npm audit` shows no high finding.
- [ ] **Step 2:** `docs/plugins.md`, `HANDOVER.md`, UI README note the page. **Step 3:** `make check`, UI tests and build. **Step 4:** commit.
