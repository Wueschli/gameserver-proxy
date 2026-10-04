# HA-replicated tunnel address allocation and live HA membership

Date: 2026-10-03 · Status: built (2026-10-03); revised after the first spec review on PR #25 · Follows the
[tunnel address authority](2026-10-02-tunnel-address-authority-design.md) spec, whose
"HA-replicated allocation" non-goal this resolves, and closes the HANDOVER row "Change a
live HA member's address".

## Intent

Today `--tunnel-network` together with `--ha-peers` is refused at startup
(`addresses::resolve_flags`), and in pin-only mode under `--ha-peers` uniqueness holds per
controller node only. The reason is structural, not just the allocator's lock: the two
peer registries (`peers`, `proxy_peers`) and the address book are per-node `sled`
databases outside the Raft group. An origin that registers on node A is invisible to an
edge subscribed to node B.

Make the registries and the address book part of the replicated state machine, so a
controller tier with `--ha-peers` allocates tunnel addresses, survives the loss of a
minority of its nodes without stopping registrations, and never hands out one address
twice. Also let an operator grow, shrink and re-address a running cluster, and turn a
single allocating controller into an HA cluster without losing its addresses.

Success:

- `--tunnel-network` with `--ha-peers` starts, and a three-node cluster behind no load
  balancer serves an origin registered on node 1 to an edge subscribed to node 3.
- Killing the leader mid-run: registrations continue on the survivors, no address is
  duplicated, and an edge that reconnects to another node resumes with its `since` cursor.
- A single controller with allocated addresses is restarted with `--ha-peers`; every
  address carries over.
- A fourth controller joins a running cluster with `--ha-join` and one admin call, and a
  member's address changes without bootstrapping a new cluster.

## Decisions made while designing (2026-10-03, with the owner)

- **Replicate the registries and the address book together** (not the address book
  alone). A claim is atomic with its registration, and per-node registries would keep the
  cross-node visibility gap.
- **Only real changes go through Raft.** A new or changed registration, a release, and a
  last-seen refresh at most once an hour are Raft entries. An unchanged re-registration
  (every 30 s per client) is answered from the local copy. Rejected: every re-registration
  through Raft (about 2 entries per peer per minute, turning snapshot install into the
  hot path for every lagging follower); per-node `last_seen` (stale warnings would differ
  per node).
- **Import on upgrade.** A fresh cluster imports one node's pre-HA registrations and
  address book once, from whichever node holds them, not from whichever node wins the
  first election (see Import). Rejected: starting empty (clients without a pinned
  address would get new addresses on their next restart).
- **Live membership changes are in scope**: join, add, remove, change address.
- **Deterministic apply.** A Raft entry carries the *request*; every replica runs the
  same claim in its state machine. Rejected: the leader decides the address and replicas
  store it — two claims in flight before the first applies can pick the same address,
  because the leader's applied state lags its log.

## Non-goals / Future work

- Changing `--tunnel-network` on a cluster that already has allocations (already a
  deferred minor of the address authority: stored addresses are not re-validated).
- A CLI verb or a `gsp-ui` page for membership. The HTTP routes plus a `curl` recipe in
  `docs/12` cover it.
- HA combined with `--role slave` (still refused; unchanged, see `crate::ha`'s module doc).
- Tuning the snapshot policy beyond openraft's defaults. Snapshots become first-class
  here (see Snapshots); their thresholds stay at the library defaults.
- Carrying over a tombstone the old single node wrote that an edge had not received yet
  (see Import, "Known gap").
- Automatic lease expiry, release from `gsp-ui`, live address change of a *peer*
  (agent/proxy) — unchanged deferred items of the address authority.

## Replicated state

### New Raft entries

`ha::WriteRequest` gains, next to `Config`, `Intent` and `Promote`:

| Entry | Carries | Applies |
|-------|---------|---------|
| `Register { role, registration, now }` | the parsed, validated registration as received (address optional), the leader's clock | address-book claim, backend expansion, registry put |
| `Release { role, name }` | | registry tombstone, address-book release |
| `Touch { role, name, now }` | | `last_seen = now` in the address book; no registry revision |
| `Import(ImportContent)` | see Import | seeds the registries and the address book once |
| `SetTunnelNetwork(Option<network>)` | the leader's `--tunnel-network`, or none (pin-only) | records the cluster's network once (see below) |

`role` is the existing `addresses::Role` (`origin` → `peers`, `proxy` → `proxy_peers`).
`now` comes from the leader proposing the entry, so `first_seen`/`last_seen` are identical
on every replica. Membership changes use openraft's own membership entries, not a
`WriteRequest`.

`WriteResponse` grows from `{ revision }` to an enum the HTTP layer maps back to today's
status codes and bodies:

- `Revision(Option<u64>)` — config, intent, promote, and Raft-internal entries (as today).
- `Registered { revision, address }` → `200 {revision, tunnel_address, tunnel_network}`.
- `Released { revision, address: Option<IpAddr> }` → `200 {revision, released}`.
- `Rejected(Rejection)` → `409` / `422` / `503` exactly as `claim_error_response` maps
  them today, including the backend-host `422` (and its "the claim is kept" behaviour).
  `Rejection` holds only the **deterministic** outcomes: address held, owner has a
  different address, outside the network, not a host address, no network (pin-only),
  network exhausted, backend host mismatch, and `NotInitialized` (a registry entry
  applied before the cluster recorded its network; `503` "cluster is initializing its
  registries"; deterministic, so it cannot crash-loop every node the way a
  `StorageError` would). `ClaimError::Storage` is never a
  `Rejection`: in apply a storage failure is returned as openraft's `StorageError`, which
  stops that node, because turning one replica's disk error into a response would let
  replicas silently diverge.
- `NotFound` → `404` for a `Release` of an unknown name.

### Deterministic apply

The state machine (`ha::state_machine`) holds an `Arc<PeersState>`, an
`Arc<ProxyPeersState>` and the shared `Arc<AddressBook>` next to the config and intent
states, and applies registry entries through the same functions the handlers use today
(`AddressBook::claim`, `expand_backends`, `register`/`remove`). Everything those functions
decide depends only on the replicated state and the entry: lowest-free allocation is
deterministic, and `now` and the network come from replicated values, never from the
applying node. So every replica reaches the same address, the same error, and the same
revision number.

### Crash-idempotent apply (a pre-existing bug this fixes)

`apply` writes a revision into a `Store`, then records `last_applied_log` in the HA meta
tree once per batch (`ha/state_machine.rs` `apply`/`write_meta`). A crash in between makes
openraft re-apply the entry on restart, and that replica gets an extra revision —
revision numbers then differ between replicas, which breaks a subscriber's `since`
cursor across nodes. Registrations write far more often than config, so the window
matters now.

Fix: every `sled` database the state machine writes (config, intent, peers, proxy-peers,
tunnel-addresses) records an `applied_index` key **in the same transaction** as the step
it belongs to, and a step whose database's `applied_index` is already at or past the
entry's log index is **skipped entirely**: neither evaluated nor written. Every entry
advances the `applied_index` of **every database it could touch**, whatever the outcome:
a step that rejects, is skipped because an earlier step rejected, or decides "no change"
still advances its database's index (in a transaction of its own), so a replayed rejection is never re-evaluated against later
state, where it might succeed. Concretely:

- `Store::put` gets an applied-index variant (`put_applied(bytes, index)`) that writes the
  revision, the current pointer and `applied_index` in one transaction.
- The registries' `current` tree moves into that same transaction (today `register` and
  `remove` update it in a second, separate write after `Store::put`), and so do the config
  state's sibling trees (`stage`, actor) for config entries.
- `AddressBook::claim`/`release`/`touch` take the index and write it in their existing
  two-tree transaction, together with a `last_outcome` record: the claim's result
  (assignment or `Rejection`) for that index. Rejections write `applied_index` and
  `last_outcome` only.

An entry that touches two databases (a `Register` claims in tunnel-addresses, then puts in
a registry) runs its steps in a fixed order. On replay after a crash between them, the
book step is skipped and the registry step reads the book's `last_outcome` for the
entry's index (entries apply one at a time, so only the latest entry can be half-applied)
to decide whether and with which address to write. Replayed entries need no response:
openraft only routes `client_write` responses for entries proposed in the live term, so
nothing waits on a replay.

### The cluster's tunnel network

Apply depends on the network, so it must be one replicated value, not each node's flag.
Before the first registry write of a fresh cluster, the leader records the network
with exactly one entry: `Import` (carrying the source node's network) when a node holds
pre-HA data, else `SetTunnelNetwork(<its --tunnel-network or none>)`; Import, "Choosing
the source", says how the leader decides. Both set the replicated `initialized` marker,
so neither is proposed again by a later leader.

Afterwards a node whose `--tunnel-network` differs from the recorded value (a different
network, one where none is recorded, or the reverse) logs an `ERROR` naming both and
answers registry writes with `503` naming both, until its flag is fixed. Changing the recorded network is a
non-goal. Pin-only mode under HA is the "recorded: none" case and works the same
way, but now with cluster-wide uniqueness, so its startup warning goes away.

**Interaction with IPv6 (parallel spec).** The IPv6 planning thread keeps one address
family per controller tier, configurable, with IPv6 documented as the default for the
automatically managed in-tunnel pool, while origins can still be addressed by IPv4 on the
underlay. This design is family-agnostic by construction:

- `SetTunnelNetwork` and `Import` carry the network as its CIDR string; the family is
  whatever that string is. "One family per tier" becomes "one recorded network per
  cluster", enforced by the mismatch rule above.
- Entries, responses and snapshots carry addresses as strings / `IpAddr`, never
  `Ipv4Addr` or a fixed four-byte key. Whichever spec lands second adapts the
  address-book types (`Network`, the `by_address` key encoding, `Assignment::address`);
  this spec's entry and snapshot formats need no change for that.
- The IPv6 spec (PR #24) keeps lowest-free allocation as a linear scan bounded by its
  65 534-entry cap. That is fine inside apply too (at most that many `sled` lookups, and
  only when nearly full); deterministic apply only requires the allocator to stay a pure
  function of the replicated state.
- PR #24's `--tunnel-readdress` changes the network of a running controller. Until network
  changes are in scope here, `--tunnel-readdress` together with `--ha-peers` or
  `--ha-join` is refused at startup; whichever of the two specs lands second adds the
  refusal.
- All comparisons in this design (the unchanged check, a requested address against a
  stored one, a node's network against the recorded one) compare **parsed** values
  (`IpAddr`, `SocketAddr`, `Network`), never strings, so `[fd49:0::2]:25565` and
  `[fd49::2]:25565` are the same backend and don't send every re-registration through
  Raft.

### Reads

`GET /peers`, `GET /peers/{name}`, `GET /proxy-peers…`, both subscribe streams and
`GET /tunnel/addresses` are served by whichever node receives them, from its local
replica — the relaxed-read rule `docs/10` already applies to config. Revision numbers are
identical on every replica, so an edge that reconnects to another node resumes with its
cursor. A cursor ahead of a lagging follower's head just waits for the tail (today's
`subscribe_worker` behaviour). The daily stale `WARN` is logged by the leader only.

### Snapshots

The premise "the log is never purged" in today's comments is wrong. `main.rs` builds
`openraft::Config { ..Default::default() }`, and openraft 0.9.25 defaults to
`snapshot_policy: LogsSinceLast(5000)`, `max_in_snapshot_log_to_keep: 1000` and
`replication_lag_threshold: 5000`, and `ha/log_store.rs` implements `purge`. Config writes
never reached 5 000 entries; registrations and hourly touches will (100 peers reach it in
about two days on touches alone). After that a `--ha-join` learner, or any follower more
than 5 000 entries behind, is caught up by snapshot, so the snapshot path becomes a main
path. This spec makes snapshots first-class, in build slice 1:

- `build_snapshot` includes config and intent revisions *with their numbers and stage /
  actor metadata*, both registries as `(revision, bytes)` pairs plus their `current` maps,
  the address-book entries, the recorded network, the `initialized` marker, and each
  database's `applied_index`.
- `install_snapshot` **replaces** each store: clear it, then write every revision at its
  stored number (a registry log can start above 1 after Import), in one transaction per
  database. Today's install replays into existing stores without clearing them and
  renumbers from 1, which gives a non-empty lagging follower duplicate revisions.
- `get_current_snapshot` returns the last built snapshot, persisted next to the HA meta
  tree, instead of `None`.
- openraft spawns `build_snapshot` in parallel with `apply` (`core/sm/worker.rs`, "the
  builder must hold a consistent view"). Today's builder is the live `Arc` and reads the
  stores while later entries apply, so a snapshot could carry an `applied_index` newer
  than its content, and a follower installing it would skip an entry for good.
  `get_snapshot_builder` (which runs on the state-machine worker, serialized with
  `apply`) therefore copies everything into an owned value first: every store's
  revisions, the `current` maps, the address book, every `applied_index`, the cluster
  state and the HA meta (`last_applied_log`, membership). `build_snapshot` only
  serializes that copy.
- The misleading comments in `ha/state_machine.rs` and the "log grows unbounded" note in
  `docs/10` are corrected.

Snapshot size is bounded by the registries' logs. Each registry compacts its log
(2026-10-04): every 1024 writes, and on the first write after a start, it drops each entry a later one for the same name
supersedes, so the log holds one entry per name ever registered, its registration or the
tombstone that removed it. Revision numbers are never reassigned, so the log has gaps and
a `since` cursor still yields everything that changed after it. Tombstones stay because
subscribers keep their view across reconnects and replay from `since=0`: one that missed a
removal must still be told. Growth is bounded by distinct names, not by changes.
Compaction is local to each replica (revision numbers are the same on every replica
whether or not it has compacted yet), so it needs no Raft entry.

## Write path

### Handlers

`POST /peers` / `POST /proxy-peers` under HA:

1. Parse and `validate()` locally, exactly as today (`422` before anything is proposed).
2. **Unchanged check, on any node.** Normalize the incoming registration the way apply
   would store it: fill in the stored address when none was requested, expand `:port`
   backends against it, and parse addresses into canonical values. It is unchanged when
   the normalized registration **equals the stored one, field for field** (the whole
   struct, so `ProxyRegistration.boot_id`, which changes on every proxy restart and is how
   agents learn to drop a dead WireGuard session, counts, and so does any field added
   later):
   - `last_seen` younger than one hour → answer `200` from the local copy (current
     revision, address, recorded network). No Raft traffic.
   - older → answer `200` from the local copy right away and propose `Touch` in the
     background (forwarded to the leader if this node is not it). No quorum degrades to
     a missed touch, never to a failed or delayed re-registration.
   - This also holds on a node whose `--tunnel-network` mismatches the recorded one: an
     unchanged re-registration is a read and is answered `200`; only writes get the
     mismatch `503`.
3. Otherwise propose `Register` through `ha::client::propose_write`, which forwards the
   original body to the leader's `/peers` (or `/proxy-peers`) when this node is not the
   leader. No quorum → `503`, as config writes do today.

`DELETE /peers/{name}` / `DELETE /proxy-peers/{name}` always propose `Release`.

`propose_write` currently builds `{revision}` itself; it becomes generic over a
`WriteResponse → Response` mapper so each route keeps its own response shape. Forwarding
is unchanged (the leader's handler re-runs steps 1–3 itself).

Accepted staleness: a lagging follower can answer "unchanged" for a name the leader just
released. The client's next tick (or the tombstone on its subscription) corrects it —
the same staleness reads already accept.

### Without HA

No change. Single-node controllers keep calling `claim`/`register` directly; they gain
only the transactional `current` tree and nothing else.

## Import (single node → HA cluster)

### Setting the pre-HA data aside

At startup with `--ha-peers` or `--ha-join`, each of `peers/`, `proxy-peers/` and
`tunnel-addresses/` that holds data but no `applied_index` marker is pre-HA data. The node
renames it to `<dir>.pre-ha` (never deletes it) and opens a fresh database in its place.
A node that later finds a marker treats the directory as replicated state.

### Choosing the source

The leader of a fresh cluster must not decide from its own disk alone: in the usual
upgrade (the old controller restarted with `--ha-peers` next to two empty nodes) an empty
node wins the first election about two times in three, and `SetTunnelNetwork` would then
set `initialized` and lose every address.

- `/raft/whoami` (peer-token gated, see Live membership) also reports `pre_ha`: whether
  this node set aside pre-HA data, and its counts.
- While `initialized` is unset, the leader proposes no registry entry and answers
  registry writes `503` ("cluster is initializing its registries"), until it has a
  `whoami` answer from **every voter** in the membership. Then:
  - no node has pre-HA data → `SetTunnelNetwork`;
  - exactly one node has it → the leader fetches that node's content with
    `GET /raft/pre-ha` (peer-token gated) and proposes `Import(ImportContent)`;
  - more than one node has it (today's pin-only `--ha-peers` deployments, where each node
    kept its own registries) → nothing is initialized; the leader logs an `ERROR` naming
    the nodes and their counts, and registry writes stay `503` until the operator restarts
    the nodes with `--ha-import-source <node-id>`.
- `--ha-import-source <node-id|none>` (same value on every node, like `--ha-peers`) names
  the source explicitly; with it the leader waits only for that node (or for nobody with
  `none`), so a voter that is down during the upgrade cannot block it. The other nodes'
  set-aside data is left untouched with a `WARN`. Their clients' pins come back within
  30 s; a pin that only ever collided across nodes now gets `409`, and the upgrade notes
  in `docs/12` say so.

`ImportContent` = the current registration per name for each registry, the address-book
entries (`role`, `name`, `address`, `first_seen`, `last_seen`), the source's tunnel
network, and each registry's last revision number. It does **not** copy the registries'
full logs (the 30-second history could be millions of entries); it re-writes each
current registration as a new revision.

Apply: if `initialized` is already set, the entry is a no-op (so a second leader racing an
import cannot import twice). Otherwise it seeds the address book, records the network,
starts each registry log at **the source's last revision + 1** and writes the current
registrations from there, then sets `initialized`. Continuing the numbering keeps every
edge's `since` cursor from the single-node days valid: an edge whose cursor is past a new
log's head would otherwise miss events silently (`peers/api.rs` `subscribe_worker` skips
everything `<= since`).

A follower that also set aside `.pre-ha` data logs a `WARN` when it applies an `Import`
naming the directory it left untouched.

### Known gap

A tombstone the old node wrote before the import, which a subscribed edge had not
received yet, is not carried over; that edge keeps the removed peer until it restarts.
Copying the full history would close it at the cost above; noted, not done.

## Live membership

### Joining

`gsp-controller --ha-join --ha-node-id <id> --ha-token <t>` starts a node that **never
calls `raft.initialize`**. It serves `/raft/*` and waits to be added. `--ha-join` and
`--ha-peers` are mutually exclusive (startup error). Today every node with an empty log
calls `initialize` with its `--ha-peers`, so a replacement node started with the original
`--ha-peers` and empty data could disrupt or split the cluster; `docs/12` will say a new
or replacement node must use `--ha-join`. A joined node restarts with `--ha-join` again
(its log is non-empty, so `initialize` would be a no-op anyway).

### Admin routes

Gated by `--auth-token` like `/admin/adopt`, forwarded to the leader like other writes,
logged at `info` with their `X-Actor`:

- `GET /admin/ha/members` → voters, learners (id and address each), current leader id.
  Served locally by any node.
- `POST /admin/ha/members {id, addr}` → identity check (below), `add_learner(id, addr,
  blocking = true)` (waits until the learner has caught up, by log entries or by snapshot once
  the log has been purged), then `change_membership(AddVoterIds{id})`. A large catch-up
  can outlast the 10 s write-forward timeout, so a forwarded `POST` uses its own longer
  bound (`MEMBER_ADD_TIMEOUT`, 5 min) instead of `FORWARD_TIMEOUT`. `409` if `id` is
  already a voter.
- `DELETE /admin/ha/members/{id}` → `change_membership(RemoveVoters{id}, retain =
  false)`, which removes the node entirely. `422` when it would remove the last voter; `404`
  for an unknown id. Removing the current leader is allowed (openraft steps it down after
  the change commits).
- `PUT /admin/ha/members/{id} {addr}` → identity check, then
  `change_membership(SetNodes{id: addr})`.

### Identity check

openraft warns that `SetNodes` pointing a node at another node's address can produce two
leaders (`docs/cluster_control/dynamic-membership.md`, "Update Node"). Before `POST` and
`PUT`, the leader calls `GET <addr>/raft/whoami` (behind `--ha-token`), which returns the
node's `--ha-node-id`, whether its log is empty, and its `pre_ha` summary (see Import). The change is refused with `422`
unless the id matches; a `POST` additionally requires an empty log or a node that is
already a learner of this cluster. Unreachable → `503`.

### Docs

The "static at bootstrap, dynamic membership deferred" paragraph in `docs/10` and the
`docs/12` "HA replicas over TLS" note about bootstrapping a new cluster are rewritten;
moving a cluster to `https://` peers becomes a `PUT` per member.

## Errors

Unchanged for registrations (`409`/`422`/`503`/`404` with `{"error": …}`). New:

- `503` "tunnel network mismatch: this node has X, the cluster recorded Y".
- `503` "cluster is initializing its registries" (waiting for `whoami` answers, or
  several nodes hold pre-HA data and no `--ha-import-source` is set).
- `503` with the existing "no raft leader elected yet; retry shortly" for writes without
  quorum (unchanged text).
- Membership: `409` already a voter, `422` identity mismatch / last voter / non-empty
  foreign log, `404` unknown id, `503` target unreachable.

## Repository changes

- Code (`gsp-controller`): `ha/mod.rs` (entries, response enum), `ha/state_machine.rs`
  (registry + address-book apply, idempotent apply, snapshot), `ha/client.rs`
  (generic response mapping), new `ha/members.rs` (admin routes, whoami), `ha/routes.rs`
  (`/raft/whoami` with `pre_ha`, `/raft/pre-ha`), new shared registry core (extracted from
  `peers/api.rs` and `proxy_peers/api.rs`), `store.rs` (`put_applied`, start-at revision), `addresses.rs`
  (index-aware claim/release/touch, `IpAddr`-ready entry formats), `peers*.rs`,
  `proxy_peers*.rs` (HA write path, transactional `current`), `main.rs` (drop the
  `resolve_flags` HA refusal and the pin-only warning, `--ha-join`, pre-HA set-aside,
  import source choice, `--ha-import-source`, network recording, a snapshot policy
  tests can lower).
- Tests: `gsp-fleet-tests` (`tests/ha_tunnel_addresses.rs`, reusing the `ha_tls.rs`
  cluster helpers).
- Docs: `docs/10` (registries join the replicated state; membership), `docs/11`
  ("Address authority": HA), `docs/12` (HA + allocation, `--ha-join`, membership recipe),
  `AGENTS.md` module list, `HANDOVER.md` (remove the "HA-replicated allocation" piece of
  the deferred row and the "Change a live HA member's address" row; add residuals),
  `docs/superpowers/README.md` index.

## Testing

TDD throughout; each layer's test fails first.

1. **State machine (unit):** two state machines fed the same entries end with identical
   registries, address books and revision numbers; 64 interleaved `Register` entries
   never share an address; each `Rejected` outcome matches the non-HA handler's; replaying
   any suffix of applied entries (simulated crash before `write_meta`) changes nothing,
   including a replayed rejection that would succeed against later state; a crash between
   the book step and the registry step completes from `last_outcome`; a storage failure
   in apply is a `StorageError`, never a `Rejected`; `Touch` changes `last_seen` only.
2. **Snapshots (unit + fleet):** install into a *non-empty* follower replaces its stores
   (no duplicate revisions) and keeps revision numbers above 1; with a low
   `LogsSinceLast` in a test config, a snapshot is built, the log purged, and a node that
   joins afterwards catches up by snapshot.
3. **Import (unit + fleet):** applies once, a second `Import` is a no-op; registry logs continue
   at source head + 1; a subscriber with an old cursor receives the imported
   registrations; set-aside renames only unmarked, non-empty directories; the source is
   chosen from `whoami` answers whichever node leads (fleet: the data node is forced to be
   a follower); several sources without `--ha-import-source` initialize nothing.
4. **Handlers (in-module, single-node Raft):** unchanged re-registration proposes nothing
   (count proposals), including one spelled with a non-canonical IPv6 address; a stale
   `last_seen` answers at once and proposes one `Touch`; a changed backend proposes
   `Register`; a changed `boot_id` proposes `Register`; response shapes and status codes equal today's for every path; network
   mismatch → `503`.
5. **Membership (in-module + whoami):** id mismatch refused; last voter refused;
   `--ha-join` with `--ha-peers` is a startup error; a `--ha-join` node never initializes.
6. **Fleet (`gsp-fleet-tests`, real processes):** three nodes with `--tunnel-network`:
   register on node 1, subscribe on node 3, see it; kill the leader, register a new
   origin on a survivor, no duplicate address, and a subscriber reconnecting to another
   node with its cursor misses nothing; single node with allocations restarted as one
   member of a fresh cluster keeps every address; a fourth node joins via `--ha-join` and
   `POST /admin/ha/members`, then the leader is removed and the cluster keeps writing; a
   `PUT` to a new port moves a member.
7. **Tunnel e2e:** unchanged scenarios stay green (single controller). No new netns
   scenario — the fleet tests cover the controller side, and clients see no wire change.

## Build order

Each slice green under `make check` before the next:

1. Crash-idempotent apply and first-class snapshots for config and intent
   (`put_applied`, sibling trees in the transaction, replacing install, persisted current
   snapshot) — fixes the two existing bugs alone.
2. Extract a shared registry core from `peers` and `proxy_peers` (today about 60%
   identical, 913 and 854 lines in `api.rs`), so every later change lands once;
   transactional `current` in it; index-aware address book; `IpAddr`-ready entry formats.
3. Registry entries in the state machine (`Register`/`Release`/`Touch`,
   `SetTunnelNetwork`, the `initialized` marker), response enum, HA write path with the unchanged check; drop the
   startup refusal.
4. Snapshot content for registries and the address book.
5. Import (set-aside, `whoami`'s `pre_ha`, source choice, `--ha-import-source`, apply).
6. Membership (`--ha-join`, whoami, admin routes).
7. Fleet tests for the end-to-end scenarios, then docs.

## Review focus

Determinism of apply (no `now_secs()`, no local flag read inside apply); the idempotent
skip returning the same response; the unchanged check comparing expanded backends; the
import source choice when an empty node leads; a registry write reaching a leader before the `initialized` marker commits; the
identity check against split brain; removing the leader; a replacement node started
without `--ha-join`.

## Points the owner may want to change

- The one-hour `last_seen` refresh (vs. the 14-day stale default it serves).
- Answering an unchanged re-registration `200` without quorum.
- `PUT` for address changes vs. remove-then-add.
- Copying the full registry history in Import to close the tombstone gap.
