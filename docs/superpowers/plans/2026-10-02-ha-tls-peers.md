# TLS-capable HA peers + readable HTTP errors Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `--ha-peers` accepts `id=https://host[:port]` so replica traffic can cross a TLS terminator, and every outbound-HTTP error in the fleet shows its cause chain.

**Architecture:** One helper builds every peer URL (`ha::peer_url`); `--ha-peers` parsing moves into the `ha` module, validates and normalises URL-form entries. `gsp_http::error_chain` renders an error plus its sources; all reqwest-error-to-text sites use it.

**Tech Stack:** Rust, openraft (`BasicNode.addr: String`), reqwest 0.12 (`reqwest::Url`), tokio-rustls test terminator (`gsp_fleet_tests::tls_front`).

**Spec:** `docs/superpowers/specs/2026-10-02-ha-tls-peers-design.md`

## Global Constraints

- The legacy `id=host:port` form must produce exactly `http://host:port{path}` as today, and is not newly validated.
- URL-form entries: scheme `http`/`https`, host required, path empty or `/`, no query/fragment/userinfo; error text starts `--ha-peers entry "<entry>": `.
- Raft log/snapshot formats unchanged (`BasicNode.addr` stays a string).
- `gsp-http` stays reqwest + thiserror only.
- `make check` green before every commit; `cargo fmt --all` as its own step first.

## Review Focus

1. Trailing slash / mixed case (`HTTPS://Localhost:8443/`) — stored normalised as `https://localhost:8443`, no `//raft`. → Task 2 `url_entries_are_normalised`.
2. URL without a port (`https://ctl.example`) — accepted, default port implied, stored `https://ctl.example`. → Task 2 `url_entries_are_normalised`.
3. Legacy IPv6 `2=[::1]:9911` — unchanged, `http://[::1]:9911/raft/vote`. → Task 2 `legacy_entries_stay_plain_http`.
4. A node whose `--ha-node-id` is absent from `--ha-peers` (self_addr falls back to `--listen`) — behaviour unchanged; no test change (existing code path untouched).
5. reqwest repeating itself in the chain ("error sending request … : error sending request …") — deduplicated. → Task 1 `error_chain_skips_repeated_sources`.

---

### Task 1: `gsp_http::error_chain` and readable errors everywhere

**Files:**
- Modify: `crates/gsp-http/src/lib.rs`, `crates/gsp-http/tests/tls.rs`
- Modify (text-rendering of reqwest errors only): `crates/gsp/src/{controller_client,intent_client,tunnel_client,aggregator_client,proxy_register,resolver,discovery}.rs`, `crates/gsp-agent/src/{register,proxy_subscribe}.rs`, `crates/gsp-controller/src/{parent_client,intent/relay,ha/client,ha/network}.rs`, `crates/gsp-aggregator/src/{parent_push,fanout}.rs`, `crates/gsp-ui/src/{fleet_feed,aggregator_proxy,controller_proxy}.rs` — whichever of these turn a `reqwest::Error` into text.

**Interfaces:**
- Produces: `pub fn error_chain(e: &(dyn std::error::Error + 'static)) -> String`.

- [ ] **Step 1: failing tests** in `tests/tls.rs`:
  - `error_chain_names_the_tls_cause`: plain client (`builder_with(&[])`) request to the fixture server → `let s = error_chain(&err);` assert `s.to_lowercase()` contains `"certificate"` or `"unknownissuer"`, and `err.to_string()` does **not** (proves the helper adds information).
  - `error_chain_skips_repeated_sources`: a local error type chain `Outer("a: b") -> Inner("b")` → `error_chain` == `"a: b"`; chain `Outer("a") -> Inner("b")` → `"a: b"`.
- [ ] **Step 2:** `cargo test -p gsp-http` → compile FAIL (`error_chain` missing).
- [ ] **Step 3:** implement: walk `source()`, append `": " + text` unless the accumulated string already ends with / contains that text.
- [ ] **Step 4:** `cargo test -p gsp-http` → PASS.
- [ ] **Step 5:** replace each `anyhow!("…: {e}")` / `error = %e` whose `e` is a `reqwest::Error` with `gsp_http::error_chain(&e)` (find them with `grep -rn 'map_err(|e|' ` and the `error = %e` warn/error sites; check each `e`'s type). Leave `controller_client`'s `with_context` form.
- [ ] **Step 6:** `cargo fmt --all && make check`; commit `feat(gsp-http): error_chain — every HTTP error shows its cause`.

### Task 2: `ha::peer_url` + `ha::parse_peers`, proven by a 3-replica TLS cluster

**Files:**
- Create: `crates/gsp-controller/src/ha/peers.rs` (`pub mod peers;` in `ha/mod.rs`)
- Create: `crates/gsp-fleet-tests/tests/ha_tls.rs`
- Modify: `crates/gsp-controller/src/main.rs` (drop `parse_ha_peers`, call `ha::peers::parse_peers`; `--ha-peers` help: "`id=host:port` (plain HTTP) or `id=https://host[:port]` (e.g. behind a TLS terminator; trusts `--ca-file`)"), `ha/network.rs:45`, `ha/client.rs:83`.

**Interfaces:**
- Consumes: `gsp_fleet_tests::{spawn_controller_with, tls_front::{tls_front, TEST_CA}, free_port, minimal_gsp_config, wait_until}`.
- Produces: `pub fn peer_url(addr: &str, path: &str) -> String`; `pub fn parse_peers(raw: &[String]) -> anyhow::Result<BTreeMap<NodeId, openraft::BasicNode>>`.

- [ ] **Step 1: e2e test** `three_replicas_replicate_through_tls_terminators` in `ha_tls.rs`: for i in 1..=3 pick plain port `p_i`, start `tls_front(127.0.0.1:p_i)` → `f_i`, spawn controller i with `--listen 127.0.0.1:p_i --ha-node-id i --ha-peers 1=https://localhost:f1,2=https://localhost:f2,3=https://localhost:f3 --ca-file TEST_CA`. Retry `POST http://127.0.0.1:p1/config` (`minimal_gsp_config`) until 2xx (≤60 s). Then one `POST /config` to each `p_i` with distinct bodies → each 2xx (≥2 of them are followers, so forwarding over https is exercised). Then `wait_until` (≤30 s) every `GET /config` returns the highest posted `x-config-revision` and its body.
- [ ] **Step 2:** run it → RED: today the URL is glued after `http://` (`http://https://localhost:…`), so no leader is ever elected and the first loop times out.
- [ ] **Step 3: unit tests** in `peers.rs`:
  - `legacy_entries_stay_plain_http`: `peer_url("127.0.0.1:9901","/raft/vote") == "http://127.0.0.1:9901/raft/vote"`; `peer_url("[::1]:9911","/raft/vote") == "http://[::1]:9911/raft/vote"`; `parse_peers(["1=127.0.0.1:9901"])[1].addr == "127.0.0.1:9901"`.
  - `url_entries_are_normalised`: `["1=HTTPS://Localhost:8443/"]` → addr `"https://localhost:8443"`; `["2=https://ctl.example"]` → `"https://ctl.example"`; `peer_url("https://localhost:8443","/raft/append") == "https://localhost:8443/raft/append"`.
  - `bad_url_entries_are_rejected`: `1=ftp://h:1`, `1=https://h:1/x`, `1=https://h:1/?q=1`, `1=https://u@h:1` → each `Err` starting `--ha-peers entry "<entry>": `.
  - `entries_without_an_id_are_rejected`: `["127.0.0.1:1"]` → `Err` containing `is not id=host:port`.
- [ ] **Step 4:** `cargo test -p gsp-controller ha::peers` → compile FAIL.
- [ ] **Step 5:** implement (normalise as `Url::to_string()` minus one trailing `/`); switch `network.rs` and `client.rs` to `peer_url`.
- [ ] **Step 6:** `cargo test -p gsp-controller` and `cargo test -p gsp-fleet-tests --test ha_tls` → PASS.
- [ ] **Step 7: negative e2e** `replicas_without_the_ca_never_elect_a_leader`: same cluster without `--ca-file`; every `POST /config` to `p1` over 8 s is non-2xx. (Asserts the outcome only — whether the raft error reaches the log depends on openraft's log level, not this change.) Run → PASS on first run is expected here; it guards against verification being off, which Step 6's positive test cannot.
- [ ] **Step 8:** `cargo fmt --all && make check`; commit `feat(controller): --ha-peers accepts https:// base URLs; 3-replica TLS e2e`.

### Task 3: Docs

**Files:** `docs/12-deployment.md` (HA over TLS subsection: URL-form peers + `--ca-file`, terminator in front of each replica's listener, **bootstrap-only** limit; delete the "HA and adoption traffic stays plain HTTP" limit, correct adopt; fix the stray `: ` line from the previous review), `docs/10-distributed-control-plane.md` (`--ha-peers` form, if it documents the flag), `HANDOVER.md` (replace "TLS for HA / adopt traffic" row with a "change a live member's address" row; drop the "Terse reqwest errors" row; trim the `--ca-file` minors row of what this branch fixed; refresh "Resume here" + "Most recent landings"; native-TLS umbrella now = controller-served TLS only).

- [ ] **Step 1:** edit; every claim cites the test that proves it (`ha_tls.rs`).
- [ ] **Step 2:** `make check`; commit `docs: HA peers over TLS, adopt correction, handover`.
