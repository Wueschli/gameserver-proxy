# Component Upgrades Implementation Plan (#185 phase B)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans (project convention: no subagents in implementation threads). Steps use checkbox (`- [ ]`) syntax.

**Goal:** Operators can upgrade a fleet component by component: peers advertise protocol versions, newer minors are only used against peers that understand them, the UI flags version skew, and `docs/upgrading.md` gives the supported order and runbook.

**Architecture:** Builds on Phase A (`X-Wayhouse-Protocol` header, gossip version byte, `schema_version`, store marker). A small `PeerVersions` map in `wayhouse-http` remembers the last protocol version each peer answered with; call sites ask `peer_supports(peer, minor)` before sending new optional fields. `wayhouse_build_info` gains a `protocol` label; the aggregator and UI surface per-node version and flag skew. A fleet test starts nodes with an injected older minor.

**Tech Stack:** Rust, axum, prometheus metrics, React, `crates/wayhouse-fleet-tests`.

**Spec:** `docs/superpowers/specs/2026-10-05-component-versioning-design.md`. Prerequisites merged: `2026-10-05-protocol-and-config-versions.md`, v0.1.0 tagged; **#186 (draining UDP worker drops new sessions) fixed before the proxy runbook is declared supported**, because proxy upgrades rely on draining; #88 (Kubernetes) for the rolling-update section only.

## Global Constraints

- Window: protocol major must match; minors are additive; the product-level window is N / N-1 minor versions (0.N and 0.(N-1)). Any change that would break the window bumps `PROTOCOL_MAJOR` **and** ships a shim for the previous major for one product minor (the shim is a future plan; this plan only documents and tests the rule).
- Test-only override: env `WAYHOUSE_TEST_PROTOCOL_MINOR` honoured only in `cfg(test)` or behind a cargo feature `test-protocol-override` used by `wayhouse-fleet-tests` (never in release images; `deploy-lint` or a grep test asserts the feature is not enabled by the Dockerfile).
- Versions stay 0.x; this plan adds no user-visible config.
- `make check` and the fleet tests pass.

## Review Focus

- A proxy older by one minor than its controller must keep working with the controller sending only baseline fields (assert on the wire in the fleet test, not just "no error").
- A skew flag must not flap: red only after a mismatch counter increased in the last 5 minutes, yellow whenever versions differ.
- The upgrade doc's order must be consistent with the code: a test reads `docs/upgrading.md` table rows against `PROTOCOL_MAJOR`/`MINOR` and the product version (script test).
- Controller tiers (parent to slave, relay upward) and raft HA inside a tier are different mechanisms; the runbook and tests treat them separately. Raft follower-first is exercised by a fleet test if raft test support allows; otherwise the runbook says it is untested.
- The docs must not claim the N / N-1 window is enforced by the header check (it checks majors only); say what is tested: injected minors always, previous-release images in `compat` once two releases exist.

---

### Task 1: Peer version memory and capability gating

**Files:**
- Create: `crates/wayhouse-http/src/peers.rs`
- Modify: `crates/wayhouse-http/src/lib.rs`, `protocol.rs` (client middleware records the response header), no call site changes here; Task 3 adds the first real use.

**Interfaces:**
- Consumes: `ProtocolVersion` (`wayhouse_config::version`), `PeerProtocol` request extension from the Phase A layer (the receiver's version as seen by a server).
- Produces: both directions of gating: server side `pub fn receiver_supports(ext: &PeerProtocol, minor: u16) -> bool` (a handler or SSE stream checks it once on connect, per stream), client side `pub struct PeerVersions` (cheap `Clone`, `Arc<Mutex<HashMap<String, ProtocolVersion>>>`), `pub fn record(&self, peer: &str, v: ProtocolVersion)`, `pub fn get(&self, peer: &str) -> Option<ProtocolVersion>`, `pub fn peer_supports(&self, peer: &str, minor: u16) -> bool` (unknown peer = false: baseline until the first response), `pub fn forget(&self, peer: &str)`; a `reqwest` response hook helper `pub fn observe(&self, peer: &str, resp: &reqwest::Response)`.

- [ ] **Step 1: Write failing tests**: `unknown_peer_does_not_support_new_minor`, `records_and_gates_by_minor`, `older_response_lowers_support` (peer downgraded), `receiver_supports_reads_the_request_extension`, `missing_extension_means_baseline`, `observe_reads_the_header`, `garbage_header_is_ignored`.
- [ ] **Step 2: Run** `cargo test -p wayhouse-http peers`. Expected: FAIL. **Step 3: Implement. Step 4: Run.** Expected: PASS. **Commit** `feat: remember peer protocol versions (#185)`.

### Task 2: Build info carries the protocol; the aggregator and UI show skew

**Files:**
- Modify: `crates/wayhouse-http/src/metrics.rs` (`set_build_info` adds label `protocol`), every `wayhouse_build_info` assertion (grep: `crates/wayhouse-fleet-tests/tests/fleet_metrics.rs`, `crates/wayhouse/src/main.rs`, controller crate), `crates/wayhouse-aggregator/src/ingest.rs` (payload gains `version: String` and `protocol: String`, both `#[serde(default)]`), `api.rs` (instances listing carries them), `crates/wayhouse/src/aggregator_client.rs` (send them), `crates/wayhouse-ui/web/src/pages/FleetPage.tsx` (+ test), `types.ts`

**Interfaces:**
- Produces: metric label `protocol="1.0"`; `IngestPayload.version`, `IngestPayload.protocol`; UI skew model `skew(instances) -> "none" | "within-window" | "outside-window"` computed in the aggregator (`GET /fleet/instances` gains `skew` per instance relative to the fleet's controller or highest version) and shown as a badge.

- [ ] **Step 1: Write failing tests**: `build_info_has_protocol_label`, `ingest_payload_without_version_still_parses` (old proxies), `aggregator_marks_within_and_outside_window`, UI `FleetPage` tests `shows_version_and_protocol_per_instance`, `flags_skew_within_window_yellow_outside_red`.
- [ ] **Step 2: Run** `cargo test -p wayhouse-http metrics && cargo test -p wayhouse-aggregator skew && make ui-test`. Expected: FAIL.
- [ ] **Step 3: Implement**; "within window" = same major, product minor differs by at most 1; "outside" = other major or minor differs by more than 1 or the node's mismatch counter rose recently (from the metric the proxy already reports, or a `protocol_mismatches` field in the payload: choose the payload field to avoid scraping).
- [ ] **Step 4: Run** the commands plus `make check`. **Commit** `feat: fleet view shows component versions and skew (#185)`.

### Task 3: Fleet tests for mixed versions

**Files:**
- Create: `crates/wayhouse-fleet-tests/tests/mixed_versions.rs`, `.github/workflows/compat.yml` (non-required: once a previous release exists, runs the previous release's images against the current ones in the compose demo and `make deploy-smoke`; before that it is a no-op with a notice)
- Modify: the crates' `Cargo.toml` for the `test-protocol-override` feature, `wayhouse-config::version` (the override hook)

**Interfaces:**
- Consumes: the fleet-test harness in `crates/wayhouse-fleet-tests/src/lib.rs` (look at `tests/fleet.rs` for starting a controller and a proxy).
- Produces: tests `proxy_one_minor_behind_registers_and_gets_baseline_fields` (the controller's response body to the older proxy contains no field newer than that minor; assert via a recorded exchange), `other_major_proxy_is_refused_with_readable_log_and_counter`, `slave_tier_older_than_parent_gets_baseline_fields` (parent tier with a child tier whose injected minor is older; this is the tier path, distinct from raft), `raft_follower_first_upgrade_then_leader_change` (two controllers in one tier, if raft test support allows).

- [ ] **Step 1: Write the failing tests** as above, plus the build assertion `release_images_do_not_enable_test_protocol_override` (a shell-level check in `deploy/lint.sh`).
- [ ] **Step 2: Run** `cargo test -p wayhouse-fleet-tests mixed_versions --features test-protocol-override`. Expected: FAIL.
- [ ] **Step 3: Implement** the override hook and whatever small server-side change the first test shows is needed (use `peer_supports` at one real call site, the smallest optional field available at that time, to prove the mechanism end to end; if none exists, add a documented no-op optional field `capabilities` to the proxy registration response that lists supported features, which later changes can extend).
- [ ] **Step 4: Run** the same command and `make check`. **Commit** `test: mixed-version fleet behaviour (#185)`.

### Task 4: `docs/upgrading.md`

**Files:**
- Create: `docs/upgrading.md`, `.github/scripts/check_upgrading_doc.py`, `.github/scripts/check_upgrading_doc_test.py`
- Modify: `docs/README.md`, `docs/10-distributed-control-plane.md` (link), `deploy/README.md` (link), `AGENTS.md` (row: changing `PROTOCOL_*` or `CONFIG_SCHEMA_VERSION` requires a row in `docs/upgrading.md`), `.github/workflows/ci.yml` (run the script in the `docs` job from the docs-overhaul plan, or in `release-policy` if that job does not exist)

**Interfaces:**
- Produces: `docs/upgrading.md` sections: Compatibility rules (window, majors, minors), Version table (rows: product version, protocol `major.minor`, config `schema_version`, store format, ABI; one row per release, newest first), Order of operations (the four steps from the spec), Controller tiers (leaf slave tiers first, then the root), raft HA inside a tier (followers first, move leadership, former leader last), Proxy runbook (drain, upgrade, rejoin; link to the #186 fix), Agents, Kubernetes (rolling update with readiness; flagged "pending #88" until it lands), Rollback, Troubleshooting (the 426 message and what it means, the config `schema_version` error, the store `FormatTooNew` error). `check_upgrading_doc.py` fails when the newest table row does not equal the current `PROTOCOL_MAJOR.MINOR`, `CONFIG_SCHEMA_VERSION` and `STORE_FORMAT` constants parsed from the Rust source (regex on `pub const`), and warns for the product version.

- [ ] **Step 1: Write failing tests** for the script: `passes_when_table_matches_constants`, `fails_when_protocol_row_is_stale`, `fails_when_no_table`.
- [ ] **Step 2: Run** `python3 .github/scripts/check_upgrading_doc_test.py`. Expected: FAIL. **Step 3: Implement the script and write the doc. Step 4: Run** both tests and the script. Expected: PASS.
- [ ] **Step 5: Commit** `docs: upgrading a fleet, with a checked version table (#185)`.

---

## Self-review

Spec coverage: window and rules (doc plus tests), response-header echo used for gating (Task 1, 3), visibility and skew (Task 2), order of operations and HA runbook (Task 4, Task 3 third test), Kubernetes tie-in to #88 as a doc line only. Not implemented by design: a protocol-major shim (no second major exists yet), raft RPC gating beyond the header (open question in the spec).
