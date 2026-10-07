# Sniffer Install From a Registry (UI Backend and Sniffers Page) Implementation Plan (#183 phase C)

> **Terminology update (2026-10-07).** Leandro split the old "plugin" concept in two. **Sniffers** are the WASM protocol/hostname sniffer modules and live in [`wayhouse-proxy/sniffers`](https://github.com/wayhouse-proxy/sniffers). **Plugins** are integrations with other systems (e.g. the Pelican panel, #213) and live in [`wayhouse-proxy/plugins`](https://github.com/wayhouse-proxy/plugins); their design is still open. Wherever this document says "plugin" or "plugins repo" for a WASM sniffer module, read **sniffer** / **sniffers repo**. Code identifiers, crates and paths were renamed to "sniffer" in the same PR as this banner; older text below may still show the old names.


> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans (project convention: no subagents in implementation threads). Steps use checkbox (`- [ ]`) syntax.

**Goal:** From the Sniffers page an operator browses the official registry and external registries, installs a verified sniffer on the fleet, and sees what it will run with, with an "at your own risk" gate for external registries.

**Architecture:** `wayhouse-ui` backend gains a registry client (`registry_client.rs`: fetch index, fetch artifact, with HTTPS-only and size and time caps), a persisted registry list (`registries.rs`), and routes `/api/registries/*` that use `wayhouse-registry` for validation, compatibility and verification, then reuse the existing fleet upload fan-out (`POST /api/fleet/sniffers`, i.e. `aggregator_proxy::upload_sniffer`) to deliver the module. Proxies keep only their upload endpoint (re-validated there, #171) and need no internet access. The React page `PluginsPage.tsx` is extended.

**Tech Stack:** Rust (axum, reqwest through `wayhouse_http`), `wayhouse-registry`, React/TypeScript with vitest and Testing Library.

**Spec:** `docs/superpowers/specs/2026-10-05-plugin-registry-design.md` ("Where the client lives", "Trust"). Prerequisites merged: `2026-10-05-registry-crate-and-index.md`, `2026-10-05-sniffer-upload-validation.md`.

## Global Constraints

- Registry URLs `https://` only; redirects followed only to `https://` (reqwest `redirect::Policy::custom`, max 5); timeouts 10 s connect and 30 s total; index body cap 1 MiB; artifact cap `min(size from index, 8 MiB)`; never log tokens or full response bodies.
- Mutating routes (`POST`/`DELETE` under `/api/registries`) require the same session role as other mutating UI routes (see `role.rs` and how `/api/fleet/sniffers` is guarded; do not invent new auth); read routes need an authenticated session.
- Persisted registry list: JSON file `--registries-file <path>` (default: unset, in-memory only with a warning in the UI that changes are lost on restart); atomic write (temp file + rename), file mode 0600, max 32 registries.
- Default registry: `https://raw.githubusercontent.com/wayhouse-proxy/sniffers/main/index.json`; removable; disabled entirely by `--no-default-registry`. (Confirm the final index location during the bootstrap plan; it is a constant in one place.)
- Official public key constant lives in `crates/wayhouse-ui/src/registry_keys.rs` as `OFFICIAL_PUBKEY: Option<&str>`; `None` until the maintainer provides the key (**manual step**), in which case signatures are shown as "cannot verify" and installs proceed unsigned (first-cut rule: signatures optional).
- Warnings are part of the API contract: responses for external registries carry `"risk": "external"`; the UI must show the warning on add and before every install.
- `make check`, `make ui-test` pass.

## Review Focus

- SSRF: a registry URL **or a redirect from a registry** (`302` to `https://127.0.0.1/...`) is refused; the guard sits in the resolver, so it applies to every hop. A registry URL pointing at `https://127.0.0.1/...`, `https://169.254.169.254/...` or `https://[::1]/` is refused (private, loopback, link-local, unspecified, unique-local ranges), with the test using literal IPs and a resolver-level guard for hostnames that resolve to those (custom `reqwest` resolver or connect-time check on the resolved addr).
- A registry that serves a huge or never-ending body must be cut off by the caps, with a clear error.
- Install for a version whose `abi` does not match must not upload anything, and the response says why.
- A partial fan-out (3 of 5 proxies accepted) is reported per instance, never as success.
- Two operators adding the same registry URL: second add is a no-op with a 200 and the same id, not a duplicate.
- Pinned instances (non-empty `settings.sniffers.modules`) answer `409 pinned` from the proxy and write nothing (upload-validation plan, Task 3); the UI shows the pin line to add per instance and offers to install again afterwards. A refused instance is not a failure of the install overall: report it as `pinned`.

---

### Task 1: Registry list and persistence

**Files:**
- Create: `crates/wayhouse-ui/src/registries.rs`
- Modify: `crates/wayhouse-ui/src/main.rs` (flags `--registries-file`, `--no-default-registry`), `lib.rs` (module), `api.rs` (`AppState` gets `registries: Arc<Registries>`)

**Interfaces:**
- Produces: `pub struct RegistryRef { pub id: String, pub name: String, pub url: String, pub official: bool }` (`id` = first 12 hex of sha256 of the normalized URL); `pub struct Registries` with `pub fn load(path: Option<PathBuf>, include_default: bool) -> Result<Self, RegistriesError>`, `pub fn list(&self) -> Vec<RegistryRef>`, `pub fn add(&self, url: &str, name: Option<&str>) -> Result<RegistryRef, RegistriesError>` (validates `https`, host present, no credentials in the URL, normalizes), `pub fn remove(&self, id: &str) -> Result<bool, RegistriesError>`, `pub fn get(&self, id: &str) -> Option<RegistryRef>`.

- [ ] **Step 1: Write failing tests**: `default_registry_is_listed_and_official`, `no_default_flag_hides_it`, `add_rejects_http`, `add_rejects_userinfo_url`, `add_is_idempotent_same_id`, `add_caps_at_32`, `remove_unknown_is_false`, `persists_atomically_and_reloads` (temp dir; check file mode 0600), `corrupt_file_is_an_error_not_silent_reset`.
- [ ] **Step 2: Run** `cargo test -p wayhouse-ui registries`. Expected: FAIL.
- [ ] **Step 3: Implement** with a `Mutex<Vec<RegistryRef>>` and temp+rename writes.
- [ ] **Step 4: Run.** Expected: PASS. **Commit** `feat(ui): persisted list of plugin registries (#183)`.

### Task 2: Registry client with network guards

**Files:**
- Create: `crates/wayhouse-ui/src/registry_client.rs`
- Test: same file with a local axum server (plain `http` allowed only under `cfg(test)` through a `RegistryClient::for_tests()` constructor that disables the https-only and private-address guards; production constructor cannot)

**Interfaces:**
- Consumes: `wayhouse_http::builder()`, `wayhouse_registry::{parse_index, Index}`.
- Produces: `pub struct RegistryClient`, `pub fn new() -> Self`, `pub async fn fetch_index(&self, url: &str) -> Result<Index, FetchError>` (in-memory cache keyed by URL, 5 minutes, `pub fn invalidate(&self, url: &str)`), `pub async fn fetch_artifact(&self, url: &str, max_bytes: u64) -> Result<Vec<u8>, FetchError>`, `pub async fn fetch_signature(&self, url: &str) -> Result<Vec<u8>, FetchError>` (max 4 KiB), `pub enum FetchError { Scheme, PrivateAddress(IpAddr), Timeout, TooLarge, Status(u16), Index(IndexError), Io(String) }`.

- [ ] **Step 1: Write failing tests**: `fetches_and_parses_index`, `index_is_cached_for_five_minutes` (injectable clock or a request counter on the test server with a zero TTL variant), `oversize_index_is_cut_off`, `artifact_larger_than_max_bytes_is_cut_off_while_streaming` (server sends endless body; client stops at the cap), `non_200_is_status_error`, `refuses_private_literal_ips` (127.0.0.1, 10.0.0.1, 169.254.169.254, ::1, fc00::1, 0.0.0.0), `refuses_https_to_http_redirect`, `refuses_redirect_to_private_address`, `slow_server_times_out`.
- [ ] **Step 2: Run** `cargo test -p wayhouse-ui registry_client`. Expected: FAIL.
- [ ] **Step 3: Implement** streaming reads with a byte counter, redirect policy as in Global Constraints, and the address guard via a custom DNS resolver wrapper that filters resolved addresses.
- [ ] **Step 4: Run.** Expected: PASS. **Commit** `feat(ui): guarded registry HTTP client (#183)`.

### Task 3: Registry routes: browse and install

**Files:**
- Create: `crates/wayhouse-ui/src/registry_api.rs`
- Modify: `crates/wayhouse-ui/src/lib.rs` (router), `registry_keys.rs` (create), `aggregator_proxy.rs` (extract a reusable `pub(crate) async fn fan_out_upload(state, name, bytes, actor) -> Result<Vec<InstanceResult>, ProxyError>` from `upload_sniffer`; behaviour of the old route unchanged, covered by its existing tests)

**Interfaces:**
- Consumes: `Registries`, `RegistryClient`, `wayhouse_registry::{select, Environment, verify_artifact}`, fleet instance versions from the aggregator (`GET /fleet/instances` already proxied; read each instance's `wayhouse_build_info` version if exposed there, else treat `proxy_versions` as `[]` and skip the `min_proxy` check with a visible note; check `crates/wayhouse-aggregator/src/api.rs` for the field).
- Produces (JSON):
  - `GET /api/registries` -> `[{id,name,url,official,risk}]`
  - `POST /api/registries {url,name?}` -> `RegistryRef`; `DELETE /api/registries/{id}` -> 204/404
  - `GET /api/registries/{id}/plugins` -> index sniffers with, per sniffer, `compatible: {version, reason?}` computed by `select`
  - `POST /api/registries/{id}/install {name, version?}` -> `{plugin, version, signed, risk, results:[{instance, ok, error?}], pinned_instances:[{instance, pin:{name,sha256}}]}`; status `200` when every instance accepted, `207` when partial, `422` for compatibility or verification failure with `{"error": "<reason>"}`, `502` when the registry is unreachable.

- [ ] **Step 1: Write failing tests** with a fake registry server and a fake aggregator (the existing `aggregator_proxy` tests show the pattern): `lists_registries_with_risk_flag`, `add_then_list_then_delete`, `plugins_listing_marks_incompatible_with_reason`, `min_proxy_check_is_skipped_with_a_note_when_no_instance_versions`, `install_happy_path_uploads_verified_bytes_under_the_plugin_name`, `install_refuses_sha_mismatch_and_uploads_nothing`, `install_refuses_abi_mismatch_and_uploads_nothing`, `install_refuses_module_with_import`, `install_reports_partial_fanout_as_207`, `install_reports_pinned_instance_from_409_without_marking_it_failed`, `install_external_registry_response_carries_risk_external`, `mutating_routes_need_the_mutating_role` (reuse the role test helpers), `install_unknown_registry_is_404`.
- [ ] **Step 2: Run** `cargo test -p wayhouse-ui registry_api`. Expected: FAIL.
- [ ] **Step 3: Implement** the handlers; install order: select version, fetch artifact, fetch signature if listed, `verify_artifact` (key from `registry_keys`), then `fan_out_upload`. Log `actor`, registry id, sniffer, version and result at info.
- [ ] **Step 4: Run** the same command and `cargo test -p wayhouse-ui`. Expected: PASS. **Commit** `feat(ui): browse registries and install verified plugins (#183)`.

### Task 4: Sniffers page

**Files:**
- Modify: `crates/wayhouse-ui/web/src/pages/PluginsPage.tsx`, `PluginsPage.test.tsx`, `api.ts`, `api.test.ts`, `types.ts`
- Create: `crates/wayhouse-ui/web/src/components/RegistryRiskDialog.tsx` (+ test)

**Interfaces:**
- Consumes: the Task 3 JSON routes.
- Produces: UI states: registry list with "official" badge and "external, at your own risk" badge; "Add registry" form with the risk dialog (must be confirmed); sniffer table (name, description, versions, compatible/incompatible with reason, signed/unsigned); install button opening a dialog that shows `limits.max_memory_bytes`, `limits.call_timeout_ms`, the `config` documentation and, for external, the risk text; per-instance result list after install; for `pinned_instances` the exact pin snippet with a copy button.

- [ ] **Step 1: Write failing tests** (vitest + Testing Library, same style as the page's existing tests with a mocked `fetch`): `lists_registries_and_marks_external`, `adding_external_registry_requires_confirming_the_risk`, `incompatible_plugin_shows_reason_and_disables_install`, `install_dialog_shows_limits_and_config`, `external_install_dialog_shows_risk_text`, `partial_install_result_lists_failed_instances`, `pinned_instance_shows_pin_snippet`.
- [ ] **Step 2: Run** `make ui-test`. Expected: FAIL.
- [ ] **Step 3: Implement** the UI; keep the existing manual upload and delete controls (still needed for local plugins).
- [ ] **Step 4: Run** `make ui-test` and `make ui` (type check and build). Expected: PASS. **Commit** `feat(ui): install plugins from a registry on the Plugins page (#183)`.

### Task 5: Docs and manual items

**Files:**
- Modify: `docs/plugins.md` (operator section: adding registries, trust, risk, pinned instances), `docs/06-operations-observability.md` if metrics were added (none planned), `deploy/README.md` (UI flags), `HANDOVER.md`

- [ ] **Step 1: Document** flags, default registry, risk model, and the **maintainer to-do list**: generate the minisign key pair offline, provide `OFFICIAL_PUBKEY`, store the signing secret in the sniffers repo (next plan).
- [ ] **Step 2: Run** docs checks. **Commit** `docs: installing plugins from a registry (#183)`.

---

## Self-review

Spec coverage: client in the UI backend (deviation from the transition plan, recorded in the spec), trust checks (sha256, optional minisign, ABI and `min_proxy`), external registries with warnings, limits shown before install, pinned-instance handling. Not here: updates and rollback (next plan), populating the registry (bootstrap plan). The new key constant is `None` until the maintainer supplies it, so this plan is shippable without it.
