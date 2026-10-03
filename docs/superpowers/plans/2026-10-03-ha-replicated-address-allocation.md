# HA-Replicated Address Allocation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `gsp-controller` with `--ha-peers` replicates both peer registries and the tunnel address book through its Raft group, allocates addresses cluster-wide, imports a single node's addresses on upgrade, and lets an operator add, remove and re-address members of a running cluster.

**Architecture:** Raft entries carry registration *requests*; the state machine applies them deterministically through one shared registry core and the address book, with a per-database `applied_index` that makes every step crash-idempotent. Snapshots become first-class (openraft purges after 5 000 entries). Unchanged re-registrations are answered from the local replica; membership uses openraft's `add_learner` / `change_membership` behind `/admin/ha/members`.

**Tech Stack:** Rust (axum, sled, tokio, clap, reqwest, openraft 0.9.25 with `serde` + `storage-v2`), `cargo test`, `gsp-fleet-tests` (real processes).

**Spec:** `docs/superpowers/specs/2026-10-03-ha-replicated-address-allocation-design.md` (read it first; it is the authority for any conflict with this plan).

## Global Constraints

- Apply must be deterministic: no `now_secs()`, no read of a node-local flag, no randomness inside `RaftStateMachine::apply`. `now` and the network come from entries / replicated state.
- A storage failure inside apply is an openraft `StorageError`, never a `Rejected` response (spec: "`ClaimError::Storage` is never a `Rejection`"). Every *deterministic* failure, including a registry entry applied before initialization (`Rejection::NotInitialized` → `503`), is a `Rejected`.
- A step whose database's `applied_index >= entry index` is skipped entirely, rejections included. Every entry advances the `applied_index` of every database it could touch, whatever the outcome (`Store::mark_applied` / the book's equivalent when nothing is written).
- The snapshot builder never reads live state: `get_snapshot_builder` copies everything (serialized with `apply`), `build_snapshot` serializes the copy (openraft runs it in parallel with apply).
- Every comparison of addresses, backends and networks uses parsed values (`IpAddr`, `SocketAddr`, `Network`), never strings.
- "Unchanged" = the normalized incoming registration equals the stored one field for field (whole struct; covers `boot_id`).
- HTTP status codes and bodies for `/peers` and `/proxy-peers` stay exactly as today for every path.
- The Raft log *is* purged (openraft defaults: `LogsSinceLast(5000)`, keep 1 000, lag threshold 5 000); keep the defaults in production, lower them only in tests.
- `--ha-join` and `--ha-peers` are mutually exclusive; `--tunnel-readdress` (from the IPv6 spec, PR #24) with `--ha-peers` or `--ha-join` is refused if that flag exists when this lands.
- Non-HA single-node behaviour does not change apart from the transactional `current` tree.
- Requires `protoc` (install it in the cloud env before `make check`).
- AGENTS.md: `cargo fmt --all` as its own step, then `make check`, before every commit. Work on the thread's branch and land through a PR; the commit trailer is whatever attribution the executing session is given.
- No `unsafe`; no `#[allow]` without a one-line justification.

## Review Focus

Failure modes the spec implies that no single task's happy-path tests cover; each names the test that pins it.

1. **A proxy restart answered as "unchanged".** A new `boot_id` with everything else equal must propose `Register`, or the #18 edge-restart fix regresses. Pinned by Task 6's `a_changed_boot_id_proposes_register`.
2. **An empty node wins the first election during an upgrade.** The data node's addresses must still be imported. Pinned by Task 8's `import_happens_when_an_empty_node_leads` (fleet).
3. **A replayed rejection that would now succeed.** A `Register` rejected with `409`, replayed after the holder released, must stay rejected (skipped). Pinned by Task 3's `a_replayed_rejection_is_not_re_evaluated`.
4. **A non-empty lagging follower receives a snapshot.** It must end with exactly the leader's revisions. Pinned by Task 2's `install_replaces_a_non_empty_store`.
5. **An address spelled differently.** `[fd49:0::2]:25565` and `[fd49::2]:25565` must normalize to the same value, or every IPv6 re-registration goes through Raft once #24 lands. Pinned by Task 6's `canonical_addr_folds_equivalent_spellings` (a pure-function test, independent of the configured family).

---

### Task 1: Crash-idempotent apply for config and intent

**Files:**
- Modify: `crates/gsp-controller/src/store.rs`
- Modify: `crates/gsp-controller/src/api.rs` (`apply_revision_with_stage_and_actor`, `set_stage`, `set_actor`)
- Modify: `crates/gsp-controller/src/intent/api.rs` (`apply_revision`)
- Modify: `crates/gsp-controller/src/ha/state_machine.rs` (`apply`)

**Interfaces:**
- Produces: `pub enum Applied { Written(u64), AlreadyApplied }`; `Store::applied_index(&self) -> Result<Option<u64>, StoreError>`; `Store::put_applied(&self, bytes: RevisionBytes, index: u64) -> Result<Applied, StoreError>`; `Store::put_applied_with(&self, bytes: RevisionBytes, index: u64, siblings: &dyn Fn(u64) -> Vec<SiblingWrite<'_>>) -> Result<Applied, StoreError>` where the closure receives the revision being written and `pub struct SiblingWrite<'a> { pub tree: &'a sled::Tree, pub key: Vec<u8>, pub value: Option<Vec<u8>> }` (`None` removes) names writes to trees of the same db that join the transaction; `Store::mark_applied(&self, index: u64) -> Result<(), StoreError>` for steps that write nothing.
- Produces: `AppState::apply_entry(&self, index: u64, bytes, stage, actor: Option<&str>) -> Result<Option<u64>, StoreError>` and `IntentState::apply_entry(&self, index: u64, bytes) -> Result<Option<u64>, StoreError>` (`None` = already applied).

- [ ] **Step 1: Write the failing tests** in `ha/state_machine.rs` `tests`:
  - `replaying_an_applied_config_entry_writes_no_second_revision`: apply `normal_entry(1, Config{..})`, call `sm.config.apply_entry(1, ..)` again (simulates the replay openraft does when `write_meta` was lost) → `Ok(None)`, `store.current_revision() == Some(1)`.
  - `replaying_an_applied_intent_entry_writes_no_second_revision`: same for intent.
  - `stage_and_actor_land_in_the_revision_transaction`: after `apply_entry(1, b"pools: []", Stage::promoted(), Some("alice"))`, `stage_of(1)` is promoted and `actor_of(1) == Some("alice")`, and `applied_index() == Some(1)`.
- [ ] **Step 2: Run** `cargo test -p gsp-controller state_machine` → FAIL (`apply_entry` not defined).
- [ ] **Step 3: Implement** `put_applied` / `put_applied_with` / `mark_applied` / `applied_index` in `store.rs` (key `applied_index` in the `meta` tree, written in the same `sled` transaction as the revision; return `AlreadyApplied` when the stored index `>= index`); `apply_entry` on both states (stage and actor trees passed as siblings — they live in `Store::db()`, so one transaction covers them); `apply` calls `apply_entry(entry.log_id.index, ..)`.
- [ ] **Step 4: Run** `cargo test -p gsp-controller` → PASS.
- [ ] **Step 5: Commit** `fix(controller): make Raft apply crash-idempotent for config and intent`.

### Task 2: First-class snapshots

**Files:**
- Modify: `crates/gsp-controller/src/ha/state_machine.rs` (`SnapshotContent`, `build_snapshot`, `install_snapshot`, `get_current_snapshot`, doc comments)
- Modify: `crates/gsp-controller/src/store.rs`
- Modify: `crates/gsp-controller/src/main.rs` (raft config built by a function tests can call with a low threshold)
- Modify: `docs/10-distributed-control-plane.md` (drop "log grows unbounded, compact later")

**Interfaces:**
- Produces: `Store::replace_all(&self, revisions: &[(u64, RevisionBytes)], applied_index: Option<u64>) -> Result<(), StoreError>` (clear + write at given numbers, one transaction); `Store::all_revisions(&self) -> Result<Vec<(u64, RevisionBytes)>, StoreError>`.
- Produces: `SnapshotContent { config: Vec<RevisionSnap>, intent: Vec<(u64, Vec<u8>)>, config_applied: Option<u64>, intent_applied: Option<u64> }` with `RevisionSnap { revision: u64, bytes: Vec<u8>, stage: Stage, actor: Option<String> }` — Task 5 adds registry and address-book fields to this struct.
- Produces: `pub struct SnapshotCopy` (owned: `SnapshotContent` + `SmMeta`) and `get_snapshot_builder` returning a builder that owns a `SnapshotCopy` taken at call time; `build_snapshot` only serializes it and persists the result. Task 5 extends `SnapshotCopy` with the registries, book and cluster state.
- Produces: `pub fn raft_config(snapshot_after: u64) -> openraft::Config` in `crates/gsp-controller/src/ha/mod.rs` (production passes `5000`, which equals today's default).

- [ ] **Step 1: Write the failing tests** in `ha/state_machine.rs` `tests`:
  - `install_replaces_a_non_empty_store`: follower sm has config revisions 1..3 (`b"a"`,`b"b"`,`b"stale"`); leader sm has 1..2 (`b"a"`,`b"b"`); install leader snapshot → follower `all_revisions()` equals leader's exactly (2 entries).
  - `install_keeps_revision_numbers_and_metadata`: leader revision 1 with stage `promoted: false` and actor `"bob"` → same on follower after install.
  - `current_snapshot_is_the_last_built_one`: after `build_snapshot`, `get_current_snapshot()` returns `Some` with the same `meta.snapshot_id`, also after reopening the HA db.
  - `a_builder_is_not_affected_by_later_applies`: apply entries 1..3, take `get_snapshot_builder()`, apply entries 4..6, then `build_snapshot()` → `meta.last_log_id.index == 3`, content has exactly the 3 revisions, and `config_applied == Some(3)`. Install it into a fresh sm and apply 4..6 → equals the leader.
- [ ] **Step 2: Run** `cargo test -p gsp-controller state_machine` → FAIL.
- [ ] **Step 3: Implement** the snapshot content, the copying `get_snapshot_builder`, replacing install, persisting the last built snapshot (tree `raft_snapshot`, keys `meta` and `data`), `raft_config`; rewrite the comments that say the log is never purged.
- [ ] **Step 4: Write the failing fleet test** `crates/gsp-fleet-tests/tests/ha_snapshots.rs::a_node_that_joins_after_a_purge_catches_up_by_snapshot` — skip-able until Task 9 adds `--ha-join`; write it now with `#[ignore = "needs --ha-join (Task 9)"]`, un-ignore in Task 9. It needs a hidden test flag `--ha-snapshot-after <n>` (clap `hide = true`) feeding `raft_config`.
- [ ] **Step 5: Run** `cargo test -p gsp-controller` → PASS; `make check` green.
- [ ] **Step 6: Commit** `fix(controller): snapshots replace stores and survive log purges`.

### Task 3: Shared registry core

**Files:**
- Create: `crates/gsp-controller/src/registry.rs` (core state + log/current/subscribe logic)
- Modify: `crates/gsp-controller/src/peers/api.rs`, `crates/gsp-controller/src/proxy_peers/api.rs` (thin wrappers: route paths, registration type, role)
- Modify: `crates/gsp-controller/src/lib.rs`

**Interfaces:**
- Produces: `pub trait Registration: Serialize + DeserializeOwned + Clone + PartialEq + Send + Sync + 'static { const ROLE: Role; fn name(&self) -> &str; fn requested_address(&self) -> Option<IpAddr>; fn backends_mut(&mut self) -> Option<&mut Vec<String>>; fn set_tunnel_address(&mut self, a: IpAddr); fn validate(&self) -> Result<(), String>; }` implemented by `PeerRegistration` (backends `Some`) and `ProxyRegistration` (backends `None`).
- Produces: `pub struct RegistryState<R: Registration>` with `store`, `current`, `updates`, `auth_token`, `book`, `write_lock` (today's fields), and
  - `fn register_applied(&self, reg: &R, index: Option<u64>) -> Result<Applied, StoreError>` (log + `current` in one transaction; `index: None` for non-HA)
  - `fn remove_applied(&self, name: &str, index: Option<u64>) -> Result<Applied, StoreError>`
  - `fn current_for(&self, name: &str) -> Result<Option<(u64, R)>, StoreError>` (revision included)
  - `fn all_current`, `subscribe` handler, `catch_up` moved here unchanged.
- Consumes: Task 1's `Store::put_applied_with`, `Applied`.

- [ ] **Step 1: Write the failing test** in `registry.rs`: `current_is_written_in_the_log_transaction` — `register_applied(&reg, Some(7))` then `store.applied_index() == Some(7)` and `current_for(name)` returns revision 1; a second call with `Some(7)` returns `AlreadyApplied`.
- [ ] **Step 2: Run** `cargo test -p gsp-controller registry` → FAIL.
- [ ] **Step 3: Implement** `registry.rs` by moving the shared code out of both `api.rs` files; the existing `peers/api.rs` and `proxy_peers/api.rs` HTTP tests must pass unchanged (they are the refactor's safety net).
- [ ] **Step 4: Run** `cargo test -p gsp-controller` → PASS (all existing peers/proxy-peers tests green, no test edited except imports).
- [ ] **Step 5: Commit** `refactor(controller): one registry core for peers and proxy-peers`.

### Task 4: Index-aware address book

**Files:**
- Modify: `crates/gsp-controller/src/addresses.rs`

**Interfaces:**
- Produces: `pub enum Rejection { Held{..}, OwnerHasDifferent{..}, OutsideNetwork{..}, NotHost(..), NoNetwork, Exhausted{..}, BackendHost(String), NotInitialized }` (`NotInitialized` → `503` "cluster is initializing its registries") (`Serialize`/`Deserialize`, same messages as today's `ClaimError` variants); `ClaimError` becomes `enum ClaimError { Rejected(Rejection), Storage(String) }`, and `claim_error_response` maps `Rejected(r)` exactly as before.
- Produces: `AddressBook::claim_at(&self, role, name, requested: Option<IpAddr>, now: u64, network: Option<Network>, index: u64) -> Result<Outcome, StorageFailure>` where `enum Outcome { Granted(Assignment), Rejected(Rejection), AlreadyApplied }`; `release_at(role, name, index) -> Result<Option<Option<IpAddr>>, StorageFailure>` (`None` = already applied); `touch_at(role, name, now, index)`; `last_outcome(&self) -> Result<Option<(u64, Outcome)>, StorageFailure>`; `applied_index()`.
- `Assignment::address` and entry formats carry `IpAddr` in JSON (strings), so the IPv6 spec only changes `Network` and the `by_address` key.
- The non-HA `claim` keeps its signature and calls the same core with `index: None`.

- [ ] **Step 1: Write the failing tests** in `addresses.rs` `tests`:
  - `a_rejection_advances_the_applied_index`: holder A at `.2`; `claim_at(B, Some(.2), .., index 5)` → `Rejected(Held)`; `applied_index() == Some(5)`; `last_outcome() == Some((5, Rejected(..)))`.
  - `a_replayed_rejection_is_not_re_evaluated`: after the above, `release_at(A, 6)` then `claim_at(B, Some(.2), .., index 5)` → `AlreadyApplied` (B does **not** get `.2`).
  - `claim_uses_the_given_network_not_the_opened_one`: book opened with `None`, `claim_at(.., network: Some(10.60.0.0/24), ..)` allocates `10.60.0.1`.
  - `touch_changes_last_seen_only`.
- [ ] **Step 2: Run** `cargo test -p gsp-controller addresses` → FAIL.
- [ ] **Step 3: Implement** (`applied_index` and `last_outcome` keys in a `meta` tree of the book's db, written in the claim/release/touch transaction; a rejection writes only those two keys).
- [ ] **Step 4: Run** `cargo test -p gsp-controller` → PASS.
- [ ] **Step 5: Commit** `feat(controller): index-aware, deterministic address book`.

### Task 5: Registry entries in the state machine

**Files:**
- Modify: `crates/gsp-controller/src/ha/mod.rs` (`WriteRequest`, `WriteResponse`)
- Modify: `crates/gsp-controller/src/ha/state_machine.rs` (holds the two `RegistryState`s and the book; snapshot fields)
- Create: `crates/gsp-controller/src/ha/cluster_state.rs` (`initialized` marker + recorded network, a tree in the HA db)

**Interfaces:**
- Produces:
  ```rust
  pub enum WriteRequest { Config{..}, Intent(Vec<u8>), Promote(u64),
      RegisterOrigin { reg: PeerRegistration, now: u64 },
      RegisterProxy { reg: ProxyRegistration, now: u64 },
      Release { role: Role, name: String },
      Touch { role: Role, name: String, now: u64 },
      SetTunnelNetwork(Option<String>),
      Import(ImportContent) }   // ImportContent defined in Task 8; add the variant there
  pub enum WriteResponse { Revision(Option<u64>),
      Registered { revision: u64, address: IpAddr },
      Released { revision: u64, address: Option<IpAddr> },
      Rejected(Rejection), NotFound, Touched, Recorded }
  ```
- Produces: `ClusterState::network(&self) -> Option<Option<Network>>` (outer `None` = not initialized), `ClusterState::record(&self, network: Option<Network>, index: u64)`.
- Apply order for `Register*`: book `claim_at` → (if granted) `expand_backends` against the granted address (a mismatch is `Rejected(BackendHost)` and the claim is kept, as today) → `register_applied`. When the outcome is a rejection, the registry step calls `Store::mark_applied(index)` instead of writing. On a replay where the book is `AlreadyApplied`, use `last_outcome()` for this index. `Release` and `Touch` likewise advance both the book and the registry index whatever the outcome.
- A registry entry applied while `ClusterState::network()` is `None` is `Rejected(NotInitialized)` (both the book and the registry still advance their index). Handlers normally don't propose one before initialization (Task 6), but a leader change can race it.

- [ ] **Step 1: Write the failing tests** in `ha/state_machine.rs` `tests` (helper `sm_with_network("10.60.0.0/24")` applies `SetTunnelNetwork` at index 1):
  - `two_state_machines_fed_the_same_entries_agree`: 20 mixed `RegisterOrigin`/`RegisterProxy`/`Release`/`Touch` entries into two fresh sms → identical `all_current()`, `entries()`, revision numbers.
  - `interleaved_registrations_never_share_an_address`: 64 `RegisterOrigin` without address → 64 distinct addresses.
  - `rejections_match_the_non_ha_handler`: for held / owner-has-different / outside network / backend host → the same `Rejection` the HTTP `claim_error_response` maps to `409`/`409`/`422`/`422`.
  - `a_crash_between_book_and_registry_completes_from_last_outcome`: run book step only for index 9, then `apply` entry 9 → registry has the registration at the granted address, book unchanged.
  - `a_storage_failure_is_a_storage_error_not_a_rejection`: book db closed/dropped tree → `apply` returns `Err(StorageError)`.
  - `a_rejected_register_then_a_touch_replays_cleanly`: entry 10 `RegisterOrigin` rejected (`Held`), entry 11 `Touch` for the holder; replay 10 and 11 → no change, and both registry and book report `applied_index == Some(11)`.
  - `a_registry_entry_before_initialization_is_rejected_not_fatal`: fresh sm without `SetTunnelNetwork`, apply `RegisterOrigin` → `Ok` with `Rejected(NotInitialized)`.
  - `snapshot_round_trip_carries_registries_and_the_book`: revision numbers above 1 survive.
- [ ] **Step 2: Run** `cargo test -p gsp-controller state_machine` → FAIL.
- [ ] **Step 3: Implement** the entries, responses, `cluster_state.rs`, and snapshot fields (all copied in `get_snapshot_builder`, per Task 2) (`peers`, `proxy_peers`: revisions + current + applied; `book`: entries + applied + last_outcome; `cluster`: initialized + network).
- [ ] **Step 4: Run** `cargo test -p gsp-controller` → PASS.
- [ ] **Step 5: Commit** `feat(controller): replicate registries and the address book through Raft`.

### Task 6: HA write path and the unchanged check

**Files:**
- Modify: `crates/gsp-controller/src/registry.rs` (handlers)
- Modify: `crates/gsp-controller/src/ha/client.rs` (`propose_write` generic over a response mapper)
- Modify: `crates/gsp-controller/src/api.rs`, `intent/api.rs` (callers of `propose_write`)

**Interfaces:**
- Produces: `pub async fn propose_write<F: FnOnce(WriteResponse) -> Response>(ha: &HaHandle, req: WriteRequest, path: &str, body: String, actor: Option<&str>, map: F) -> Response`.
- Produces: `fn canonical_addr(s: &str) -> Result<String, String>` (parse as `SocketAddr` or `IpAddr`, format back); `fn normalize<R: Registration>(incoming: &R, stored_address: IpAddr) -> Result<R, String>` (fill address, expand `:port`, `canonical_addr` every address) and `fn is_unchanged<R>(incoming: &R, stored: &R, stored_address: IpAddr) -> bool` = `normalize(incoming, stored_address) == Ok(stored.clone())`.
- `const TOUCH_AFTER: Duration = Duration::from_secs(3600);`
- Background touch: `tokio::spawn` of `propose_write`/forward with the response discarded.
- `RegistryState` gains `now_fn: Arc<dyn Fn() -> u64 + Send + Sync>` (default `addresses::now_secs`) so tests control `last_seen` age; the leader stamps `now` into `Register*`/`Touch` with it.

- [ ] **Step 1: Write the failing tests** in `registry.rs` `tests` (single-node Raft helper `ha_app()` that counts proposals via a wrapper around `client_write`):
  - `canonical_addr_folds_equivalent_spellings`: `"[fd49:0::2]:25565"` and `"[fd49::2]:25565"` → equal; `"10.60.0.2:1"` unchanged; `"nonsense"` → `Err`.
  - `an_unchanged_re_registration_proposes_nothing`
  - `a_stale_last_seen_answers_at_once_and_proposes_one_touch` (advance `now_fn` by `TOUCH_AFTER + 1 s`)
  - `a_changed_backend_proposes_register`
  - `a_changed_boot_id_proposes_register` (proxy registry)
  - `ha_responses_match_the_non_ha_responses`: for each of 200 / 409 / 422 / 503 / 404 (delete unknown) the HA and non-HA apps return equal status and JSON body.
  - `a_mismatched_network_node_answers_unchanged_but_refuses_writes`: node flag `10.61.0.0/24`, recorded `10.60.0.0/24` → unchanged `200`, changed `503` with both networks in the error.
- [ ] **Step 2: Run** `cargo test -p gsp-controller registry` → FAIL.
- [ ] **Step 3: Implement** the handler flow (validate → unchanged check from the local replica → `Register*` via `propose_write`; `DELETE` → `Release`), the mismatch check against `ClusterState`, `503 "cluster is initializing its registries"` while not initialized.
- [ ] **Step 4: Run** `cargo test -p gsp-controller` → PASS.
- [ ] **Step 5: Commit** `feat(controller): HA write path for the registries`.

### Task 7: Startup wiring, network recording, drop the refusal

**Files:**
- Modify: `crates/gsp-controller/src/main.rs`
- Create: `crates/gsp-controller/src/ha/members.rs` (read-only `GET /admin/ha/members` for now)
- Modify: `crates/gsp-controller/src/addresses.rs` (`resolve_flags` loses `ha_enabled` and its refusal; its test `--tunnel-network + --ha-peers refused` is replaced)
- Create: `crates/gsp-controller/src/ha/init.rs` (leader-side initialization task)

**Interfaces:**
- Produces: `pub async fn initialize_registries(ha: Arc<HaHandle>, cluster: Arc<ClusterState>, local_network: Option<Network>, import: ImportPolicy)` — runs on every node, acts only while this node is leader and `cluster.network()` is `None`; in this task `ImportPolicy` has one variant `Never` and the task proposes `SetTunnelNetwork`. Task 8 extends it.
- The registries' and book's dbs are opened before Raft and handed to `StateMachineStore::open`.
- The pin-only HA startup warning (`main.rs:147-150`) is removed.
- `GET /admin/ha/members` (read-only, served locally) lands here so the fleet tests can find the leader; the write routes come in Task 9. Its handler lives in `ha/members.rs` (created here).
- A node whose `--tunnel-network` mismatches the recorded network logs one `ERROR` naming both when it first sees the recorded value.
- The daily stale `WARN` runs only while this node is leader: factor its loop body into `fn stale_warning_due(is_leader: bool, ..) -> Option<String>` and test `the_stale_warning_is_leader_only` (follower → `None`, leader with a stale entry → `Some`).
- If the IPv6 spec's `--tunnel-readdress` exists by now, refuse it with `--ha-peers` / `--ha-join` at startup (test `tunnel_readdress_with_ha_is_refused`).

- [ ] **Step 1: Write the failing fleet test** `crates/gsp-fleet-tests/tests/ha_tunnel_addresses.rs` (reuse `ha_tls.rs`'s cluster setup, plain `http://` peers, plus `--tunnel-network 10.60.0.0/24` on all three):
  - `an_origin_registered_on_one_node_is_seen_on_another`: `POST /peers` on node 1, `GET /peers/{name}` on node 3 within 5 s returns it with the same `tunnel_address`.
  - `losing_the_leader_keeps_registrations_working`: kill the leader (from `GET /admin/ha/members`), register a new origin on a survivor, all addresses distinct.
  - `a_subscriber_resumes_on_another_node_with_its_cursor`: subscribe on node 1, read N events, reconnect to node 2 with `since=N`, register one more → exactly that one arrives.
- [ ] **Step 2: Run** `cargo test -p gsp-fleet-tests --test ha_tunnel_addresses` → FAIL (startup refused).
- [ ] **Step 3: Implement** the wiring and `initialize_registries`.
- [ ] **Step 4: Run** the fleet test → PASS; `make check` green.
- [ ] **Step 5: Commit** `feat(controller): --tunnel-network works with --ha-peers`.

### Task 8: Import

**Files:**
- Create: `crates/gsp-controller/src/ha/import.rs` (set-aside, `ImportContent`, reading `.pre-ha` dbs)
- Modify: `crates/gsp-controller/src/ha/init.rs`, `ha/routes.rs` (`/raft/whoami`, `/raft/pre-ha`), `ha/state_machine.rs` (`Import` apply), `store.rs` (start-at revision), `main.rs` (`--ha-import-source`)

**Interfaces:**
- Produces: `pub fn set_aside_pre_ha(data_dir: &Path) -> Result<PreHaSummary>` (renames each of `peers`, `proxy-peers`, `tunnel-addresses` that is non-empty and has no `applied_index` to `<dir>.pre-ha`); `#[derive(Serialize, Deserialize)] pub struct PreHaSummary { pub origins: usize, pub proxies: usize, pub addresses: usize }` (all zero = none).
- Produces: `pub struct ImportContent { pub origins: Vec<PeerRegistration>, pub proxies: Vec<ProxyRegistration>, pub book: Vec<(Role, String, Assignment)>, pub network: Option<String>, pub origins_last_revision: u64, pub proxies_last_revision: u64 }`; `pub fn read_pre_ha(data_dir: &Path) -> Result<Option<ImportContent>>`.
- Produces: `GET /raft/whoami` → `{ "node_id": u64, "log_empty": bool, "pre_ha": PreHaSummary }`; `GET /raft/pre-ha` → `ImportContent` or `404`. Both behind `--ha-token`.
- Produces: `enum ImportPolicy { Auto, Source(NodeId), None }` from `--ha-import-source <id|none>` (absent = `Auto`).
- Produces: `Store::start_at(&self, first_revision: u64, index: u64)` — sets the next revision number of an empty log.

- [ ] **Step 1: Write the failing unit tests** in `ha/import.rs` and `ha/state_machine.rs`:
  - `set_aside_moves_only_unmarked_non_empty_dirs`
  - `import_applies_once`: a second `Import` entry → `Recorded` no-op, registries unchanged.
  - `imported_logs_continue_after_the_source_head`: source head 40 → first imported revision 41.
  - `a_subscriber_with_an_old_cursor_receives_the_imported_registrations`: subscribe with `since=40` → receives them.
  - `several_sources_without_a_policy_initialize_nothing`: `initialize_registries` decision function `choose_source(&[(NodeId, PreHaSummary)], ImportPolicy) -> Decision` returns `Decision::Blocked(vec![..])` for two non-empty summaries under `Auto`, `Import(id)` for one, `SetNetwork` for none, `Import(id)` for `Source(id)`, `SetNetwork` for `ImportPolicy::None`.
- [ ] **Step 2: Run** `cargo test -p gsp-controller import` → FAIL.
- [ ] **Step 3: Implement** set-aside at startup (before opening the dbs), the routes, `choose_source`, the leader flow (wait for `whoami` from every voter, or only the named source, then propose), `Import` apply, and a `WARN` on a follower with its own `.pre-ha` data when it applies an `Import`.
- [ ] **Step 4: Write the failing fleet tests** in `ha_tunnel_addresses.rs`:
  - `a_single_node_upgrades_without_losing_addresses`: run one controller with `--tunnel-network`, register 3 origins, stop it, restart it as node 1 of a fresh 3-node cluster → `GET /peers` on node 2 lists all 3 with their old addresses.
  - `import_happens_when_an_empty_node_leads`: same, but node 1 starts last (after nodes 2 and 3 elected a leader) → still imported.
- [ ] **Step 5: Run** the fleet tests → PASS; `make check` green.
- [ ] **Step 6: Commit** `feat(controller): import pre-HA registrations on upgrade`.

### Task 9: Live membership

**Files:**
- Modify: `crates/gsp-controller/src/ha/members.rs` (write routes)
- Modify: `crates/gsp-controller/src/main.rs` (`--ha-join`), `ha/routes.rs`, `crates/gsp-fleet-tests/tests/ha_snapshots.rs` (un-ignore)

**Interfaces:**
- Produces: routes (behind `--auth-token`, writes forwarded to the leader with `X-Actor`): `GET /admin/ha/members` → `{ "leader": Option<u64>, "voters": [{id, addr}], "learners": [{id, addr}] }`; `POST /admin/ha/members` body `{ "id": u64, "addr": String }`; `DELETE /admin/ha/members/{id}`; `PUT /admin/ha/members/{id}` body `{ "addr": String }`.
- Produces: `async fn verify_identity(client: &reqwest::Client, addr: &str, id: NodeId, ha_token: Option<&str>, for_add: bool) -> Result<(), MemberError>` (calls `/raft/whoami`; add requires `log_empty` or already a learner).
- `pub const MEMBER_ADD_TIMEOUT: Duration = Duration::from_secs(300);`: a forwarded `POST /admin/ha/members` uses a client with this bound instead of `FORWARD_TIMEOUT` (test `a_forwarded_member_add_uses_the_long_timeout` asserts the forward path picks it).
- Status codes: `409` already a voter; `422` identity mismatch / last voter / non-empty foreign log; `404` unknown id; `503` target unreachable or no leader.

- [ ] **Step 1: Write the failing tests** in `members.rs` (single-node Raft + a stub whoami server):
  - `add_refuses_an_identity_mismatch`
  - `delete_refuses_the_last_voter`
  - `put_refuses_an_address_that_answers_with_another_id`
  - and in `main.rs` tests: `ha_join_and_ha_peers_together_are_refused`.
- [ ] **Step 2: Run** `cargo test -p gsp-controller members` → FAIL.
- [ ] **Step 3: Implement** `members.rs` (`add_learner(id, BasicNode{addr}, true)` then `change_membership(ChangeMembers::AddVoterIds({id}), false)`; delete `change_membership(ChangeMembers::RemoveVoters({id}), false)`; put `change_membership(ChangeMembers::SetNodes({id: node}), false)`), `--ha-join` (never calls `initialize`).
- [ ] **Step 4: Write the failing fleet tests** in `ha_tunnel_addresses.rs`:
  - `a_fourth_node_joins_then_the_leader_is_removed`: start node 4 with `--ha-join`, `POST /admin/ha/members`, it serves `GET /peers` with the cluster's data; `DELETE` the leader → a new leader commits a registration.
  - `put_moves_a_member_to_a_new_port`: restart node 3 on a new port, `PUT` its address → it rejoins and receives new registrations.
  - Un-ignore `ha_snapshots.rs::a_node_that_joins_after_a_purge_catches_up_by_snapshot` (cluster with `--ha-snapshot-after 20`, 60 registrations, then join).
- [ ] **Step 5: Run** the fleet tests → PASS; `make check` green.
- [ ] **Step 6: Commit** `feat(controller): add, remove and re-address HA members at runtime`.

### Task 10: Docs and HANDOVER

**Files:**
- Modify: `docs/10-distributed-control-plane.md` (registries in the replicated state; dynamic membership no longer deferred), `docs/11-backend-transport.md` ("Address authority": HA), `docs/12-deployment.md` (HA + allocation, `--ha-join`, membership `curl` recipe, upgrade notes incl. cross-node `409`s and `--ha-import-source`), `AGENTS.md` (module list: `registry.rs`, `ha/{members,import,init,cluster_state}.rs`), `HANDOVER.md` (remove the HA-replicated piece of the deferred row and the "Change a live HA member's address" row; add residuals: Import tombstone gap, registry-log compaction), `docs/superpowers/README.md` (row → Built, plan link), the spec's status line.

- [ ] **Step 1: Update the docs.**
- [ ] **Step 2: Run** `make check` → green (docs-only, sanity).
- [ ] **Step 3: Commit** `docs: HA-replicated address allocation and live membership`.
