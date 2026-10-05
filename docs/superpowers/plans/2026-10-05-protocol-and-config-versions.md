# Protocol and Config Version Fields Implementation Plan (#185 phase A)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans (project convention: no subagents in implementation threads). Steps use checkbox (`- [ ]`) syntax.

**Goal:** Every component-to-component protocol, the config schema and the controller's on-disk store carry a version that a node checks, so incompatible peers refuse each other with a clear error instead of misbehaving. No upgrade logic yet.

**Architecture:** One constant `PROTOCOL_VERSION` (major.minor, starts `1.0`; protocol numbers are independent of the product version, which stays 0.x: major = incompatible wire change, minor = additive change) lives in a new `wayhouse_config::version` module (the crate every component already links). HTTP/JSON component protocols carry it in an `X-Wayhouse-Protocol` header: the shared `wayhouse_http::builder()` adds it to every outgoing request, and a tower layer from `wayhouse_http` rejects a component-facing request with a different major (`426 Upgrade Required`) and echoes the server's own version in the response header. Gossip (UDP, postcard, HMAC) gets a leading version byte inside the MAC. The config gets an optional top-level `schema_version`. The controller's sled store gets a format marker. Rule: **major mismatch refuses**; minor differences are accepted (the window rules come with the upgrade design, `2026-10-05-component-upgrades.md`).

**Tech Stack:** Rust, axum/tower, reqwest, serde, sled, postcard.

**Spec:** `docs/superpowers/specs/2026-10-05-component-versioning-design.md`; issue #185; transition plan decision 2.

## Global Constraints

- Header name exactly `X-Wayhouse-Protocol`, value `<major>.<minor>` (`1.0`). Missing header on a request is **accepted** (operator tools such as `curl` call these routes too); a present but unparsable or other-major value is refused. Responses always carry the header.
- Refusal body (plain text): `wayhouse protocol <peer> is not compatible with this node (<ours>): upgrade the older side`.
- Component-facing routes only; admin, UI and `/healthz` and `/metrics` routes are never gated. Component-facing today: controller `POST /proxy-peers` and the registry routes (`registry.rs` base routes and `/subscribe`), `GET /config/subscribe`, `/intent` and `/intent/subscribe`, `/tunnel/addresses`, `/raft/*`, the relay and parent-controller client routes, aggregator `POST /ingest`; confirm the list with `grep -n "route(" crates/wayhouse-controller/src crates/wayhouse-aggregator/src` and record it in the module doc of the layer.
- Config: `schema_version: <u32>` is the **minimum schema the document needs**; absent means `1`; `CONFIG_SCHEMA_VERSION` (starts `1`) is what this build supports. Bump rule: any PR adding a config field, even optional, bumps `CONFIG_SCHEMA_VERSION` and adds the field to `FIELD_SINCE` (a `&[(&str, u32)]` table of dotted paths in `wayhouse-config`); the parser rejects a document that uses a field whose `since` exceeds its declared `schema_version`. Value above `CONFIG_SCHEMA_VERSION` is a hard error `config schema_version N is newer than this build supports (max M)`; never half-applied, a rejected revision keeps the previous one live. `deny_unknown_fields` stays (an older node rejects a newer field loudly). Proxies report `max_config_schema` in their registration body; the controller refuses `POST /config` with `422` when the document's `schema_version` exceeds the lowest value any registered proxy reported.
- Store marker: tree/key `meta`/`format` = `1u32`; `Store::open` writes it into a fresh db and refuses a higher value with a clear error naming the path.
- Breaking changes are fine (nothing in production); no compatibility shims for the missing-header era.
- `make check`, and the fleet tests (`crates/wayhouse-fleet-tests`) pass.

## Review Focus

- A newer-major controller talking to an older proxy (and the reverse) gets a readable error in the log of the side that refuses, and a metric increments (`wayhouse_protocol_mismatch_total{peer="..."}`; follow the naming in `wayhouse-core/src/metrics_defs.rs`).
- SSE subscribe streams: the version must be checked on connect, not mid-stream; an already open stream is not torn down by a header on the next request.
- A gossip datagram with an old version byte and a **valid MAC** is dropped and counted, not parsed with postcard (no panic on garbage payload).
- A config YAML with `schema_version: "2"` (string) or `0` gives a precise error, not a serde dump.
- Reload with a too-new `schema_version` keeps the running config (reload path, not only startup).

---

### Task 1: Version module, header layer, client header

**Files:**
- Create: `crates/wayhouse-config/src/version.rs`; `crates/wayhouse-http/src/protocol.rs`
- Modify: `crates/wayhouse-config/src/lib.rs` (`pub mod version;`), `crates/wayhouse-http/src/lib.rs` (`pub mod protocol;`, `builder()` and `builder_with()` add the default header), `crates/wayhouse-http/Cargo.toml` (+ `wayhouse-config`, tower if absent)
- Test: both new files

**Interfaces:**
- Produces: `pub const PROTOCOL_MAJOR: u16 = 1; pub const PROTOCOL_MINOR: u16 = 0; pub const CONFIG_SCHEMA_VERSION: u32 = 1;` and `pub struct ProtocolVersion { pub major: u16, pub minor: u16 }` with `FromStr`/`Display`, `ProtocolVersion::CURRENT`, `fn compatible(&self, other: &Self) -> bool` (same major).
  `wayhouse_http::protocol::HEADER: &str = "x-wayhouse-protocol"`; `pub fn layer() -> impl tower::Layer<...>` (an axum `middleware::from_fn` wrapper `require_compatible_protocol`) applied by routers; it inserts the caller's parsed version into the request as an extension `PeerProtocol(Option<ProtocolVersion>)` (so handlers and SSE streams can gate fields by the receiver's version) and increments `wayhouse_protocol_mismatch_total` through a `fn(&str)` hook passed in (`layer(on_mismatch: fn(&str))`) so `wayhouse-http` need not depend on `wayhouse-core`.

- [ ] **Step 1: Write failing tests**: `version_parses_and_displays` (`"1.0"`, rejects `"0"`, `"a.b"`, `"1.2.3"`), `compatible_means_same_major`, `layer_accepts_missing_header` (axum `oneshot` request, 200), `layer_accepts_same_major_other_minor`, `layer_rejects_other_major_with_426_and_body` (body contains both versions), `layer_rejects_garbage_value`, `layer_sets_response_header_on_success`, `builder_sends_the_header` (spin an axum echo server, call through `wayhouse_http::client()`, assert the server saw `x-wayhouse-protocol: 1.0`).
- [ ] **Step 2: Run** `cargo test -p wayhouse-config version && cargo test -p wayhouse-http protocol`. Expected: FAIL.
- [ ] **Step 3: Implement** as specified. The `layer` function and its axum dependency sit behind `wayhouse-http`'s existing `server` feature (the agent is a client only and must not pull axum); the header constant, `ProtocolVersion` re-export and the client default header are feature-independent. `builder_with` and `builder` use `default_headers` so every component client sends it.
- [ ] **Step 4: Run** the same commands. Expected: PASS. **Commit** `feat: protocol version header and layer (#185)`.

### Task 2: Apply the layer to component-facing routers

**Files:**
- Modify: `crates/wayhouse-controller/src/api.rs`, `registry.rs`, `intent/api.rs`, `addresses/api.rs`, `ha/routes.rs`, `crates/wayhouse-aggregator/src/api.rs` (the ingest sub-router), the proxy's admin-side routes if any are component-facing (check `crates/wayhouse/src/admin.rs` for routes the agent or controller call), `crates/wayhouse-core/src/metrics_defs.rs` (counter name constant)
- Test: each crate's existing router tests

**Interfaces:**
- Consumes: `wayhouse_http::protocol::layer`.
- Produces: counter `wayhouse_protocol_mismatch_total` with label `route_group` in {`controller`, `aggregator`, `raft`}; the layer's hook calls `metrics::counter!`.

- [ ] **Step 1: Write failing tests** per crate: `proxy_peers_rejects_other_major_with_426` (controller), `config_subscribe_rejects_other_major` (checks the 426 happens before the SSE stream starts), `ingest_rejects_other_major` (aggregator), `raft_route_rejects_other_major`, and negative `admin_routes_ignore_the_header` (a human route such as `GET /config` with a bad header still returns 200, because humans and tools are not gated).
- [ ] **Step 2: Run** `cargo test -p wayhouse-controller protocol && cargo test -p wayhouse-aggregator protocol`. Expected: FAIL.
- [ ] **Step 3: Implement**: wrap only the component-facing sub-routers with `.layer(protocol::layer(...))`; do not wrap the merged root router.
- [ ] **Step 4: Run** the same commands and `cargo test -p wayhouse-fleet-tests`. Expected: PASS (all components talk the same version).
- [ ] **Step 5: Commit** `feat: component routes refuse a peer with another protocol major (#185)`.

### Task 3: Gossip version byte

**Files:**
- Modify: `crates/wayhouse-core/src/gossip.rs` (`tag`, `verify_and_strip` callers, the send path around line 308, the receive path), `metrics_defs.rs`
- Test: `gossip.rs` `mod tests`

**Interfaces:**
- Produces: frame = `timestamp(8) || version(1) || postcard payload || HMAC`; `const GOSSIP_VERSION: u8 = PROTOCOL_MAJOR as u8;` new `Reject::Version(u8)`; counter constant `GOSSIP_VERSION_REJECTED_TOTAL = "wayhouse_gossip_version_rejected_total"` in `metrics_defs.rs`, next to `GOSSIP_AUTH_REJECTED_TOTAL` and `GOSSIP_STALE_REJECTED_TOTAL` (the existing style is one counter per reason).

- [ ] **Step 1: Write failing tests**: `frame_round_trips_with_version_byte`, `valid_mac_but_other_version_is_dropped_as_version` (build the frame by hand with version 9), `postcard_is_not_parsed_for_other_version` (payload is random bytes; no panic and `Reject::Version`), update the existing tamper/stale tests to the new frame layout.
- [ ] **Step 2: Run** `cargo test -p wayhouse-core gossip`. Expected: FAIL.
- [ ] **Step 3: Implement** the byte inside the MACed region right after the timestamp.
- [ ] **Step 4: Run** the same command plus `cargo test -p wayhouse-core --no-default-features` (`gossip_disabled.rs` must still compile). **Commit** `feat: gossip frames carry a protocol version byte (#185)`.

### Task 4: Config `schema_version`

**Files:**
- Modify: `crates/wayhouse-config/src/schema.rs` (`RawConfig`), `parse.rs` (check after parse), `resolved.rs` (`Config.schema_version: u32`), `crates/wayhouse-config/fuzz` targets if they construct `RawConfig` (grep), `docs/05-configuration.md`, `config.example.yaml`
- Test: `crates/wayhouse-config/src/validate/tests.rs` or `parse.rs` tests; controller test for rejected submit

**Interfaces:**
- Produces: `pub const FIELD_SINCE: &[(&str, u32)] = &[];`, proxy registration body field `max_config_schema: u32` (`crates/wayhouse/src/proxy_register.rs` `ProxyRegistration`, `#[serde(default)]` on the controller's side), controller registry stores it per proxy; `RawConfig.schema_version: Option<u32>` (`#[serde(default)]`); `ConfigError::SchemaTooNew { found: u32, max: u32 }` (extend the existing error enum; keep the message text from Global Constraints); controller `POST /config` answers `422` with that message and does not store a revision.

- [ ] **Step 1: Write failing tests**: `field_since_table_covers_every_raw_field_added_after_v1` (starts empty; asserts the table's paths exist in the schema so it cannot rot), `document_using_a_field_newer_than_its_declared_schema_is_rejected` (use a test-only entry in a `cfg(test)` table), `controller_rejects_schema_above_lowest_registered_proxy_max` (422), `proxy_registration_reports_max_config_schema`, `absent_schema_version_is_current`, `schema_version_1_ok`, `schema_version_2_is_rejected_with_message`, `schema_version_zero_rejected`, `string_schema_version_rejected`, controller `submit_config_with_newer_schema_is_422_and_keeps_current_revision`, and `reload_with_newer_schema_keeps_running_config` (in `crates/wayhouse/src/reload.rs` tests, following the existing reload test style).
- [ ] **Step 2: Run** `cargo test -p wayhouse-config schema_version && cargo test -p wayhouse-controller schema_version`. Expected: FAIL.
- [ ] **Step 3: Implement**; document the field in `docs/05-configuration.md` and add `schema_version: 1` to the top of `config.example.yaml`.
- [ ] **Step 4: Run** the same commands and `cargo test -p wayhouse reload`. **Commit** `feat: config schema_version, newer versions are rejected (#185)`.

### Task 5: Controller store format marker

**Files:**
- Modify: `crates/wayhouse-controller/src/store.rs` (`Store::open`), `StoreError`
- Test: `store.rs` `mod tests`

**Interfaces:**
- Produces: `pub const STORE_FORMAT: u32 = 1;` `StoreError::FormatTooNew { path: PathBuf, found: u32, max: u32 }`; marker kept in sled tree `meta`, key `format`, value 4 bytes BE.

- [ ] **Step 1: Write failing tests**: `fresh_store_gets_the_marker`, `reopen_keeps_marker`, `store_with_higher_format_is_refused`, `pre_marker_store_with_revisions_is_adopted_as_format_1` (open a db that has revisions but no marker: write the marker; no data change; this keeps existing dev data working even though breaking is allowed).
- [ ] **Step 2: Run** `cargo test -p wayhouse-controller store::tests::format`. Expected: FAIL.
- [ ] **Step 3: Implement** in `open` after the db opens; also note HA `ha/log_store.rs` uses its own storage: leave as is and list it under "not covered" in the spec.
- [ ] **Step 4: Run** `make check`. **Commit** `feat: controller store carries a format version (#185)`.

### Task 6: Docs

**Files:**
- Modify: `docs/10-distributed-control-plane.md` (a "Versioning" subsection), `docs/06-operations-observability.md` (the two new counters), `AGENTS.md` ("when you touch X" row: wire formats -> bump `PROTOCOL_*`/`CONFIG_SCHEMA_VERSION` and `RELEASING.md` rules)

- [ ] **Step 1: Write the three edits**, stating the rule (major mismatch refuses; a breaking wire change bumps `PROTOCOL_MAJOR` even while the product is 0.x) and that the N/N-1 window is designed in `docs/superpowers/specs/2026-10-05-component-versioning-design.md`.
- [ ] **Step 2: Run** link check (`python3 .github/scripts/check_md_links.py ...` if present). **Commit** `docs: versioning of protocols, config and store (#185)`.

---

## Self-review

Spec coverage: protocol version on controller/proxy, HA relay (raft and relay routes), aggregator fan-out, gossip, config schema, on-disk store, refuse-major behaviour with a clear error. Note for #185 text: the issue speaks of gRPC; the repo's inter-component protocols are HTTP/JSON, SSE and UDP gossip (`proto/` holds only the external resolver contract, already versioned as `wayhouse.resolver.v1`). Task 2's route list must be re-derived at implementation time because routes change between rounds.
