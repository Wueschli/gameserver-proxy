# 10 – Distributed control plane (v2)

## Status

**Design only. Nothing here is built.** Targets roadmap phases 10–12
([08-roadmap.md](08-roadmap.md)). The v1 data plane and the single-instance
control plane (chapters [02](02-architecture.md), [05](05-configuration.md),
[06](06-operations-observability.md)) are unchanged by this: every mechanism
below is either an **additional writer** feeding the same validated `Snapshot`
swap, or an **additional input** to the same per-backend health flag. The hot
path gains no network call and no lock.

The controller and aggregator are each a **recursive tree of tiers** (one
tier per failure domain, down to as small as one instance), so the same
design covers a single-instance install and a globally distributed fleet with
no separate mechanism per size — see "Fleet topology" below. Control (Tier 1)
pulls root → leaf with a static `standalone`/`slave` role per tier; the
aggregator pushes leaf → root with a homogeneous schema at every hop.
**Locked for the first release** (phase 10+11 in `docs/08-roadmap.md`): one
`standalone` controller tier + one aggregator tier, `replicas: 1` each, no
`slave` role, no HA, no adoption, operator intent stays per-instance as it is
today (only structural config moves into the controller). The hierarchy,
intra-tier HA, adoption, and shared intent are phase 12 — a real, already
fully designed extension, deliberately built *after* something works
end-to-end, not before.

**Phase 12 status (2026-09-05)**: the hierarchy, both relay logs (config +
intent), and adoption (slices 1–5) are **built** — see `docs/08-roadmap.md`.
**Intra-tier HA, staged/canary rollout, and RBAC + audit are now fully
designed below (this session) but not yet built** — each gets its own
section ("Intra-tier HA", "Staged / canary rollout", "RBAC and audit") with
concrete mechanisms, wire shapes, and ADR entries (`docs/09` ADRs 21–23), the
same level of detail the now-built pieces had before their slice landed.

---

## Why

Today every proxy instance is fully independent (ADR 4):

- **Structural config** is a YAML file per instance, hot-reloaded on `SIGHUP` /
  file-watch. Distribution (image bake, ConfigMap, `consul-template`, Ansible …)
  is the operator's problem.
- **Runtime state** splits in two, and neither is shared or persisted:
  - *operator intent* — the phase-5 backend overlay (`POST`/`DELETE` a backend),
    backend admin state (`draining` / `disabled`), route hints. Lives on live
    atomics / `ArcSwap` tables. **Lost on restart.** Applied one instance at a
    time by whoever calls that instance's admin API.
  - *observed health* — the active/passive checker's `AtomicBool` per backend.
    Each instance computes it locally.

That is fine for a handful of hand-managed instances. It does not give a fleet
operator (or a GUI that "manages everything"):

- a single authoritative place to view and edit config,
- changes that apply fleet-wide and survive restarts,
- an audit trail,
- health that benefits from more than one vantage point.

This chapter plans the smallest set of additions that provide those without
touching the data path or the "one immutable snapshot" invariant.

---

## Principles

1. **The data plane does not change.** The hot path still reads a *local*
   immutable `Snapshot` plus *local* runtime state. No per-connection call to a
   store, no added lock. Everything here is control plane.
2. **One authority per fact.** Every piece of state has exactly one writer tier
   (below). No fact is writable from two places.
3. **Level-triggered everywhere.** Sources publish *the current value*; the
   instance reconciles by diffing against what it holds. Idempotent, and the
   same shape as `ListenerManager::reconcile` and the discovery adapters
   (phase 8).
4. **Degrade to last-known-good.** A control-plane outage *freezes* state, it
   never *clears* it — the same rule a flapping discovery source already has to
   obey ("don't nuke the pool").
5. **Pull, not push.** Instances subscribe / poll. An instance that was down
   catches up from its cursor on reconnect. No "did the push land" bookkeeping,
   no per-instance delivery state on the writer.

---

## The two-axis split

Map the new state onto the predicate the data plane **already** computes:

```
Backend::takes_new_sessions()  ==  is_healthy()  &&  admin_state() == Enabled
                                   └── observed ──┘   └────── intent ──────┘
```

| Axis | Question it answers | Authority | Scope | Mechanism |
|------|---------------------|-----------|-------|-----------|
| **intent** | *should* this receive traffic? | operator / config | global | ordered, durable, versioned store — **Tier 1** |
| **observed** | *can* we reach it, from here? | the instances themselves | one failure domain | gossip / anti-entropy — **Tier 2** |

Plus a third bucket that stays **purely instance-local, never shared**:
per-listener rate-limit / concurrent-connection buckets (sized per node by
design), UDP session tables (worker-local; cross-instance session handover
remains out of scope — see ADR 4), per-connection state.

The split is not a new concept bolted on — it follows the seam the code has had
since phase 5.

---

## Tier 1 — global config & intent store

### Contents

- **Structural config**: listeners, routes, pools (with their static targets),
  balancers, filters, resolvers, `settings` — everything chapter 05 puts in the
  YAML file.
- **Operator intent**:
  - backend membership overrides (the phase-5 `BackendOverlay`),
  - backend admin state (`draining` / `disabled`, plus a new fleet-wide
    `force-down`),
  - route hints (`POST /route-hint` today),
  - resolver pins (future — a manual `src → pool/target` override that outlives
    a single hint TTL).
- Fleet feature flags.

### Shape

- **One logical writer** — the controller (below). **Readers** are every proxy
  instance.
- **Ordered, versioned, durable log.** Each accepted change is a monotonic
  revision. Quicksilver-style: central write, asynchronous fan-out, and every
  instance keeps a **local materialized replica** that it reads in-process.
- A proxy **subscribes on startup**: it receives a full snapshot + a revision
  cursor, then a change stream. On disconnect it reconnects from its cursor; on
  a total store / controller outage it serves its last replica **indefinitely**.
- Each received revision goes through the **existing**
  `validate() → Snapshot::build → ArcSwap::store` path. An invalid revision is
  rejected and logged, the previous snapshot kept — byte-for-byte the behaviour
  of a bad file reload today. Non-structural intent (admin state, hints) updates
  the live atomics / tables directly, exactly as the admin API already does.

### Relationship to the YAML file

- The file **stays a first-class config source.** An instance setting
  `config_source: file | store | file+store` selects the base; in `file+store`
  the file is the structural base and the store is an overlay (matching how the
  phase-5 overlay already layers on the file).
- The store is authoritative for **intent** even in `file` mode, so a restarted
  instance recovers the overlay / hints / admin-state it had. This closes the
  "runtime mutations don't persist" gap directly.

### What it is **not**

- Not on the hot path. Not consulted per connection or per datagram.
- Not a session store. Not shared *session* state (ADR 4 stands for sessions).

---

## Tier 2 — regional health fabric

### Contents

- Per-backend **reachability as observed within one failure domain**: a small
  record per `(instance, backend)` — `up | down`, last change, check age.
- Later, optionally: per-domain capacity / load signals for smarter load
  shedding.

### Failure domain = reachability-equivalence class

- The set of instances that reach the backends over the **same network path** —
  an AZ, a region, a rack row. Set per instance (`failure_domain:` /
  `region:` in `settings`), or discovered.
- **Rationale.** Sharing "backend X is down" is only *valid* between instances
  whose view of X is the same. Across regions it is noise: a `DC1 ↔ X`
  partition says nothing about `DC2 ↔ X`, and acting on it would wrongly pull X
  for DC2.

### Shape

- **AP, not CP.** SWIM-style membership + gossiped per-backend state, or a CRDT
  (a per-`(instance, backend)` last-writer-wins register). Small, ephemeral,
  partition-tolerant, self-healing. No election. No quorum *write*.
- **Fully rebuildable.** If the fabric is empty (cold start, total partition),
  each instance simply uses its own checks — precisely today's behaviour.
- **Authenticated.** Messages are signed / carried on a mutual-TLS mesh so a
  rogue host cannot inject "everything is down".

### How an instance decides health (authority model)

Local checks stay in charge; the domain view is **quorum-weighted advice**:

- Mark **unhealthy** when *this instance* sees `fall` consecutive failures
  **OR** a quorum (e.g. > ⅔) of the domain's instances currently report it down.
- Return to **healthy** only when *this instance* sees `rise` consecutive
  successes — never on someone else's say-so (a stale "up" must not revive a
  backend this instance genuinely cannot reach).
- A Tier-1 `force-down` always wins over any Tier-2 "up".

This removes both failure modes: one flapping instance cannot poison the pool
(quorum gate on "down"), and one instance cannot ignore a domain-wide outage
(the `OR`).

---

## Fleet topology: aggregation and control form the same hierarchy, opposite directions

The goal is a single design that scales unmodified from one instance to a
globally distributed fleet — not a different mechanism per size. Both new
components generalize into a **tree of tiers** (a tier is typically a failure
domain / region, but can be as small as one instance):

- **Aggregation (metrics, health, session/pool views) is `Push`, leaf → root.**
  Each proxy pushes its own state up to its tier's aggregator; each
  aggregator, in turn, pushes (or is pulled — see below) a merged view up to
  its parent aggregator, if it has one. The schema is homogeneous: a parent
  aggregator's input (a child proxy, or a child aggregator's merged view)
  looks the same either way, so the merge logic is the same function applied
  recursively — no distinction between "a leaf" and "a subtree" at the
  parent's level.
  - **Why push, not pull, here** (a deliberate exception to principle 5): pull
    would require every parent to hold a network route *down* to every leaf,
    which is the reachability problem the whole hierarchy exists to avoid — a
    proxy behind restrictive egress-only networking, NAT, or in a partner's
    network can push out without ever accepting an inbound admin connection
    from anywhere, including its own tier's aggregator. The cost: every
    proxy/aggregator carries a small outbound client with retry/backoff and a
    short local buffer so a momentary parent hiccup doesn't drop a push.
  - Losing the parent link degrades to **"my local aggregator still has the
    full local view; the parent's view of me goes stale/absent."** No data
    loss below the cut, no effect on the data plane anywhere.

- **Control (structural config + operator intent, Tier 1) is `Pull`, root →
  leaf**, unchanged from the single-tier design above except generalized to N
  hops: a proxy subscribes to its tier's controller (full snapshot + cursor,
  then a change stream); a `slave` controller subscribes to its parent the
  same way and relays what it receives to everything under it. This is
  principle 5 applied recursively, not a new mechanism.

### Controller role: `standalone` vs. `slave` — a static, install-time fact

Every controller tier is configured, not elected, into one of two roles:

- **`standalone`** — a root authority. It accepts writes directly (config API,
  intent API) and is the top of its own subtree. A single-instance deployment
  is a `standalone` controller with no children and no parent — the
  degenerate case falls out for free, no special-casing.
- **`slave`** — has a parent (`parent_addr` + a token). It never accepts a
  write directly; every write it receives from below is forwarded up, and it
  relays the parent's revision stream to everything below it. If it loses the
  parent, it **freezes on last-known-good** — exactly the existing Tier-1 rule
  ("a control-plane outage freezes state, it never clears it"), now applying
  at every hop, not just proxy-to-single-controller.

The role is **never inferred from connectivity.** A `slave` losing its parent
does not, and must not, promote itself to `standalone` — that is precisely how
two regions would end up independently accepting conflicting intent for the
same fact during a network partition rather than a real failure (violates
principle 2, "one authority per fact"). Promotion is only ever an explicit
operator action (see Adoption, below).

### Intra-tier HA is an orthogonal knob

`standalone`/`slave` answers "does this tier have a parent." It says nothing
about how many processes back that tier. Separately, every tier picks a
**replica count**:

- **1 node** — fine for a single proxy or a small region that accepts
  freezing (not losing — the last-good state is still served) on that one
  node's failure.
- **A small consensus group (Raft/etcd-style)** — for a tier that must keep
  *accepting new writes* (if `standalone`) or *stay live for its children*
  (if `slave`) through a single node loss. This is the same mechanism either
  way; a `standalone` root cluster and a `slave` regional cluster both run
  the same consensus group internally, they just differ in whether accepted
  writes originate locally or get forwarded to a parent.

So a deployment picks two independent settings per tier —
`role: standalone | slave` × `replicas: 1..N` — never a fixed shape baked into
the software. A lone proxy: `standalone` + 1. A globally distributed fleet: a
`standalone` Raft group at the root, `slave` groups (sized to taste) at each
region.

### Adoption (deferred — not in the phase 10/11 slices)

Turning a `standalone` tier into a `slave` of a newly-introduced parent — e.g.
an operator adding a higher tier's address + token in the admin UI after the
fact, rather than at install time — is a one-time, operator-triggered role
flip, not a new mechanism: the child starts the same subscribe flow (full
snapshot + cursor + change stream) against the new parent that any `slave`
uses from boot. Two correctness details to solve *when this is built*, not
now:

- The child's own revision history must not outrank the parent's once
  adopted — post-adoption, the parent is the sole authority for every fact the
  child used to originate itself.
- Adoption should require the child to be quiescent (caught up, nothing
  in-flight) before the flip, so a write doesn't get reordered across the
  transition.

---

## The controller (`gsp-controller`)

A **new, optional service**, deployed as one tier per the topology above (root
`standalone`, or `slave` under a parent tier). Proxies never talk to each
other through it — it is the Tier-1 writer for its subtree, and (transitively)
the GUI's route to any fact in the fleet.

- **Owns its tier's Tier-1 store** — embedded (Raft in the controller), or
  backed by etcd / a SQL DB / a git repo. This is an ADR-level choice,
  deferred (see Open questions). A `slave` tier's store is a materialized
  replica of what its parent relays, not an independently-written store.
- **Config API** (accepted only at a `standalone` tier, or forwarded up by a
  `slave`). Submit a full or partial config; the controller runs the **same
  `validate()`** the proxy does, assigns a revision, publishes down its
  subtree. History, diff, one-key rollback, staged / canary rollout (a subset
  of instances take a revision first).
- **Intent API.** The phase-5 admin verbs (`drain backend`, add / remove
  backend, `route-hint`, …) but fleet-wide (within the tier's subtree) and
  persisted — each writes a Tier-1 revision instead of poking one instance.
  Direct per-instance admin stays as break-glass.
- **Auth / RBAC / audit log live here.** The proxies' own admin APIs stay
  internal-only and are locked to their controller's identity (or its
  network); a `slave` controller's link to its parent carries its own
  token, independent of any proxy-facing credential.
- **HA** is the intra-tier replica-count knob above: N controllers in one
  tier behind a leader lock for writes originating at that tier; reads are
  stateless. A whole tier's outage stops *changes for its subtree*, not the
  data plane — every instance under it serves its last replica. See
  "Intra-tier HA (design)" below for the concrete mechanism (`openraft`, one
  Raft group per tier replicating both logs).

---

## The aggregator (`gsp-aggregator`)

A **new, optional, stateless service**, one instance (or a small horizontally-
replicated group — no consensus needed, it holds no durable state) per tier.
It is the Tier-2-adjacent read path: fleet view + fan-out for the phase-5
operational verbs, entirely separate from the controller's Tier-1 write path.

- **Ingests pushes** from its tier's proxies and from any child aggregators
  (a tier's aggregator is itself a valid "leaf" to its parent aggregator — the
  homogeneous-schema point above).
- **Serves the merged view** for its own tier immediately (`GET /fleet/*`),
  and separately pushes (or is pulled, if a deployment prefers that at the
  top where reachability isn't a constraint) that merged view to its parent,
  if it has one.
- **Fans out the phase-5 intent verbs** (drain, add/remove backend,
  route-hint, drain an instance) to every instance in its subtree at once,
  reporting partial success per instance rather than failing the whole call.
- Carries **no authority** — it never decides anything, only observes and
  relays observations up, and relays operator intent verbs down to the
  instances that actually hold the atomics. The controller (Tier 1) is the
  only place a *decision* is durably recorded.

---

## Intra-tier HA (design)

Answers "how does one tier survive a node loss" — orthogonal to `standalone`/
`slave` (that answers "does this tier have a parent"). Two very different
components, two very different designs:

### The aggregator: stateless replication, no consensus

`gsp-aggregator` already carries no durable state (`IngestStore` is
in-memory, latest-write-wins, deliberately unpersisted — see its own
`lib.rs`). HA for a tier's aggregator is therefore **just running N
identical, uncoordinated replicas behind one stable address** (a VIP, DNS
round-robin, or a k8s `Service`) — no leader, no consensus, no shared
storage:

- Every proxy in the tier pushes to the tier's **one stable address**, never
  to a specific replica — whichever replica the load balancer picks that
  tick ingests the push. Same rule for a child aggregator's upward push
  (`--parent-url` names the tier's address, never a node).
- A `GET /fleet/*` read also goes through the same stable address, so a
  client sees *some* replica's view, not necessarily the freshest — replicas
  are not required to agree with each other at any instant.
- **Accepted inconsistency**: two replicas can hold different subsets of a
  tier's instances until every instance's next push interval (default 10s)
  reaches whichever replica currently answers reads. This is not a
  regression from the single-replica design — it already showed up as
  "stale/absent" in the Failure behaviour table, HA just spreads it across
  replicas instead of across time. Self-heals every push interval with zero
  coordination code.
- **Rejected**: gossiping ingested state between aggregator replicas so they
  converge faster. Adds a real distributed system (membership, anti-entropy)
  to a component whose entire design point is "carries no authority, forgets
  nothing important because everything re-arrives on the next tick" — not
  worth it for shaving one push interval off a view that was never meant to
  be strongly consistent in the first place.

### The controller: Raft, replicated log, single leader accepts writes

`gsp-controller` **does** hold durable, order-sensitive state (two revision
logs), so its HA needs an actual consensus protocol — a tier that must keep
*accepting new writes* (`standalone`) or *stay live for its children*
(`slave`) through a node loss needs agreement on "what got written and in
what order," which a stateless replica set cannot give.

- **Library: `openraft`** (a Raft implementation, storage-and-transport
  agnostic), not a hand-rolled consensus algorithm and not an external `etcd`
  dependency. Rationale, extending ADR 20's reasoning: this project has
  already chosen to embed a KV (`sled`) rather than run an extra service for
  the Tier-1 store; embedding Raft continues that "one binary, no external
  service" story instead of trading it away the moment HA enters the
  picture. `etcd` would work but reintroduces exactly the operational
  dependency `sled` was chosen to avoid, for a fleet-control-plane component
  whose own write volume is human-paced (config/intent changes, not a hot
  path) — a full etcd cluster is a lot of operational machinery for that
  load. A hand-rolled Raft was never seriously considered: Raft's subtlety
  (log matching, leader lease safety, snapshot/log truncation) is exactly
  the kind of thing worth a maintained, audited crate rather than a bespoke
  implementation for a security-relevant (per the existing "Tier 1 is a
  high-value target" security note) component.
- **One Raft group per controller *tier*, replicating both logs together.**
  Not two independent groups (one per log): a tier's config log and intent
  log already share a lifecycle (both come from the same set of peers, fail
  over together, and a `slave` tier already relays both from the same
  parent) — one group means one leader election, one peer list, one leader
  lease to reason about, instead of two that could disagree about who leads.
  The Raft log's state machine applies an entry to **whichever** of the two
  `Store`s (config or intent) the entry names; each entry carries a
  `{log: "config" | "intent", bytes}` envelope.
- **State machine = the existing `sled` `Store`, unchanged.** `openraft`'s
  storage trait needs a state machine and a log store; the state machine is
  today's `Store::put`/`get`/`revisions_after` exactly as built (slice 1),
  fed by `openraft` applying a committed entry instead of a direct HTTP
  handler call. The Raft log itself (openraft's own append-only log of
  proposals, distinct from the config/intent revision logs it carries) lives
  in a **third** `sled` tree, `<data_dir>/raft`, on every replica.
- **Writes go through Raft; reads don't.** `POST /config`, `POST /intent`,
  and `POST /admin/adopt` propose a Raft entry and wait for it to commit
  before responding (this is where "who is the leader" matters). `GET
  /config`, `/config/subscribe`, `/config/revisions*`, `GET /intent`-side
  reads, and `/healthz` are served by **whichever replica received the
  request**, straight from its own local `Store` — no `ReadIndex` /
  leader-lease read protocol. This is a deliberate relaxation of Raft's
  usual linearizable-read guarantee, justified the same way the whole
  chapter already justifies it: principle 4 ("freeze on last-known-good")
  already accepts a subscriber seeing a slightly stale replica during a
  partition; a follower a few commits behind its leader is the *same*
  acceptable staleness, not a new risk. **Never a risk of serving state
  *out of order*** — `sled`'s local apply only ever appends, so a follower's
  view is always a strict prefix of the leader's, never a divergent one.
- **Non-leader write handling: transparent HTTP forward, not a redirect.** A
  replica that receives a write it isn't the leader for forwards the request
  to the current leader over plain HTTP (`openraft` tracks the current
  leader; the follower proxies the request body byte-for-byte, same
  `forwardable_headers` pattern `gsp-ui`'s proxies already use) and relays
  the leader's response back verbatim. **Rejected**: an HTTP redirect
  (`307` + `Location`) — every existing client of this API (`gsp`,
  `gsp-ui`, a human with `curl`) would need new leader-following logic; a
  transparent forward means literally nothing downstream of this ADR needs
  to know HA exists. The one-hop latency cost only applies to writes (rare,
  human-paced), never to reads or to the data plane.
- **Raft RPCs travel over the same `axum` server, a separate route prefix**
  (`/raft/*`), gated by a **separate peer-only shared secret** (`--ha-token`,
  distinct from `--auth-token`) rather than mTLS or a new transport — mirrors
  every other cross-service credential in this fleet (`--instance-token`,
  `--parent-token`, …), which are all shared-secret bearer tokens over plain
  HTTP on a trusted internal network, not a new trust model for this one
  link. `openraft`'s async network trait is implemented as a thin
  `reqwest`-based client, matching every other outbound HTTP call in these
  crates.
- **A `slave` tier's upward relay runs only on the leader.** `parent_client`
  / `intent::relay` (slices 1 and 4) become leader-only tasks — every
  follower would otherwise apply the same parent revision independently and
  each assign it a *different* local revision number, corrupting the
  replicated log. On a leader failover, the newly-elected leader must resume
  the relay from **where the group left off, not where it personally last
  ran one** — so the relay's cursor (`last_applied_parent_revision`, one per
  log) becomes part of the replicated state machine (a small extra key per
  `Store`, committed alongside each relayed entry) instead of a
  process-local variable. This is the one genuinely new piece of state HA
  introduces beyond "replicate what already exists."
- **Cluster membership: static at bootstrap, dynamic membership deferred.**
  `--ha-peers node1=http://host1:9901,node2=http://host2:9901,...` on every
  replica forms the initial voter set (`openraft`'s single-step static
  bootstrap, not the joint-consensus dynamic membership change API). Adding
  or removing a peer from a running group needs `openraft`'s membership-
  change support and is **explicitly out of scope for the first HA slice** —
  documented as a known limitation, not silently unsupported: growing a
  tier's replica count means a coordinated restart of the whole group with a
  new `--ha-peers` list until that slice lands.
- **`replicas: 1` (today's shape) needs no code path change.** A one-node
  Raft group trivially elects itself leader and commits every entry
  immediately (no network round trip) — `openraft` handles the degenerate
  case for free, so the phase 10+11 single-controller deployment shape isn't
  a special case of the HA design, it's the `N=1` instance of it.

### What this buys, and what it costs

Per the existing Failure behaviour table, a `standalone` (or `slave`) tier's
single-node outage today "freezes changes for its subtree" — every proxy
under it keeps running on its last replica, which is already the acceptable
degradation this whole design is built around. HA's benefit is narrower than
it might sound: it turns "changes are frozen until an operator restarts the
node" into "changes keep flowing through the surviving majority," for
deployments where that freeze window is unacceptable (a large region, or a
root tier serving many children). It adds real operational cost (N processes
per tier instead of 1, a peer list to manage, a new failure mode — a lost
Raft quorum, which degrades to the *same* frozen-changes behavior a
single-node outage already has) for that benefit, which is why `docs/08`
correctly scopes it as an explicit later slice, opted into per tier via
`replicas: 2..N`, never a default.

---

## Staged / canary rollout (design)

A subset of a tier's instances take a config revision before the rest of the
fleet — "The controller" section above already named this as part of the
Config API; this is the concrete mechanism.

### Instances self-report a rollout group

Every `gsp` instance gains an optional `settings.controller.canary_group:
<string>` (default: unset, meaning "not enrolled in any canary group" — the
overwhelmingly common case, and the entire mechanism is invisible to an
instance that never sets it). `controller_client`'s `GET
/config/subscribe?since=<cursor>` gains a `&group=<canary_group>` query
parameter (omitted when unset).

**Self-reported, not independently verified** — the same trust level
`IngestPayload::admin_url` already has in the aggregator: every instance in
a tier already holds that tier's controller/aggregator credentials and is
therefore already inside the trust boundary this whole control plane
operates within. Verifying group membership against some external source of
truth would be a real feature (ties into the open "region-scoped intent"
question) but isn't needed for "let an operator try a change on the three
instances they labelled `canary` before the other three hundred."

### One log, revisions tagged with a rollout stage — not a forked history

A canary revision is **still one entry in the same monotonic config log**
(never a second, parallel history to reconcile later) — it just carries
rollout metadata alongside its bytes:

```
{ "promoted": bool, "canary_groups": ["groupA", "groupB"] }
```

stored in a new small `sled` tree (`stage`, keyed by revision number,
alongside `revisions`/`meta`) rather than folded into the revision bytes
themselves — `Store::put`'s existing content-agnostic contract (never
inspects what it's storing) stays exactly as it is; the API layer writes an
extra `stage` entry in the same transaction.

- **`POST /config`** (today's plain submission) implicitly writes
  `{promoted: true, canary_groups: []}` — **byte-for-byte today's existing
  behavior** when the feature is never used. This is the compatibility
  anchor: nothing about slices 1–5's tests or wire shape changes.
- **`POST /config?stage=canary&group=<name>`** writes
  `{promoted: false, canary_groups: [<name>]}` (repeatable — `&group=` may
  appear more than once for a multi-group canary).
- **`POST /config/promote/{revision}`** flips that revision's `promoted` to
  `true` in place — **the one exception to "every accepted change is a new
  revision"** in this whole design, deliberately: promotion isn't new
  content, it's a visibility change to content that already exists and
  already has a revision number: minting a second revision with identical
  bytes just to mark "this one's fleet-wide now" would make `GET
  /config/revisions` show two entries for one real change, and would need
  its own reconciliation if a promoted revision needs a *further* canary
  step (it doesn't — promotion is terminal).

### What a subscriber actually receives

"Current, for group G" is defined as: **the highest-numbered revision that
is either `promoted` or has `G` in its `canary_groups`.** `GET
/config?group=<G>` and the `/config/subscribe?group=<G>` catch-up range both
compute this by scanning `stage` from the current pointer backward until a
match — accepted as an **O(revisions)** operation rather than adding an
index, matching this codebase's general bias against building for a scale
problem that doesn't exist yet: this runs at human-paced write rates against
a log sized in the hundreds to low thousands of entries for any deployment
this design targets, not a hot path and not `gsp-core`. Revisit with an
index (e.g. a `sled` tree keyed by group, pointing at its latest visible
revision) if a real fleet's revision count ever makes the scan measurable.

An instance with **no `canary_group` set** behaves exactly as it does today:
it only ever sees `promoted` revisions, so an in-flight canary is completely
invisible to the rest of the fleet until someone explicitly promotes it.

### Rollback and diff are unaffected; intent is explicitly out of scope

`POST /config/rollback/{revision}` keeps re-submitting the target revision's
exact bytes as a **new, immediately-promoted** revision (a rollback is
presumably urgent, never something to stage) — no change to slice 5's
rollback semantics. `GET /config/revisions` gains `promoted` and
`canary_groups` columns; diff is unaffected (it already compares two
revisions' bytes, unrelated to visibility).

**Canary staging applies to config revisions only, not the intent log** — a
deliberate scope cut, not an oversight. An intent op is already a narrowly
targeted, single mutation (add one backend, patch one backend's state); "give
this whole document to a subset of the fleet first" has a clean meaning for
a structural config swap and no equally clean one for "apply this one
backend-add to a subset" (an add/remove is either safe everywhere it makes
sense or it isn't — there's no natural partial-rollout story for a single
op the way there is for a whole document). Revisit if a real use case
surfaces; not designed here.

---

## RBAC and audit (design)

`docs/10`'s original text placed "Auth / RBAC / audit log" on the
controller. The phase 10+11 slice-11 redesign (see `HANDOVER.md`) already
diverged from that once, for a good reason that still holds: **`gsp-ui` is
the only place a *human* ever authenticates** — the controller and
aggregator each still gate on one shared machine bearer token apiece, and
that stays true here too. So this design keeps that split and completes
it: **RBAC is enforced in `gsp-ui`**, the one place with human identity;
the controller gains only what it's missing to make the resulting audit
trail meaningful — knowing *who* (not just *that*) a revision came from.

### Multiple human identities, replacing the single shared password

`gsp-ui --ui-password` (one shared secret for every operator) is replaced by
`--users-file <path>`, a YAML list:

```yaml
users:
  - username: alice
    password_hash: "$argon2id$v=19$..."
    role: admin
  - username: bob
    password_hash: "$argon2id$v=19$..."
    role: operator
```

- **`argon2`** (the `argon2` crate, pure Rust) for password hashing — the
  conventional, currently-recommended choice for this exact job; not
  `bcrypt` (older, smaller work-factor ceiling) and not a hand-rolled
  scheme (this is precisely the kind of security-critical, well-solved
  problem this codebase already defers to a maintained crate for, the same
  reasoning ADR 16's sniffer sandboxing and this session's HA `openraft`
  choice both use).
- A companion `gsp-ui --hash-password` CLI mode (prints an argon2 hash for a
  password read from stdin) is the intended way an operator populates
  `users-file` — never handling plaintext passwords in a config file at
  rest.
- **`--ui-password` is retained** as a legacy single-shared-secret mode
  (mutually exclusive with `--users-file`) rather than removed outright:
  unlike this codebase's usual "clean break over compat shim" pre-1.0
  stance, forcing every existing single-operator deployment to mint a
  `users-file` for a single-user setup is pure friction with no correctness
  upside — RBAC only matters once there is more than one identity to
  distinguish. A single-password deployment implicitly runs as `admin`.

### Three roles, matching the verb tiers the API surface already has

- **`viewer`** — every `GET`: fleet reads, config/revision reads and diffs.
  No verb that changes anything.
- **`operator`** — `viewer` + the phase-5 intent verbs already fanned out
  through the aggregator (drain / undrain an instance, backend add / patch /
  delete, route-hint) and their controller-intent-log equivalents (slice 3).
  Cannot touch structural config or fleet topology.
- **`admin`** — `operator` + config submit / rollback / promote (staged
  rollout, above) and `POST /admin/adopt` (a role/topology change, correctly
  gated at the top).

Three roles, not a fully general permission-matrix model: every route in
`gsp-ui`'s existing surface already falls cleanly into one of these three
buckets (see `docs/06`'s "Fleet control plane" endpoint reference), so a
richer model would be solving a problem this API surface doesn't yet have.
Region/subtree-scoped roles ("operator, but only for region X") are
explicitly deferred — `gsp-ui` only ever talks to the *root* tier (per the
existing "GUI served only by the topmost tier" rule) and today's intent
verbs have no region selector to scope against (the same open question
`docs/10` already tracks as "Region-scoped intent"); scoped RBAC is only
meaningful once that lands.

### Enforcement point and session shape

`session::SessionStore`'s `HashSet<String>` (a session id is either valid or
it isn't) becomes a `HashMap<String, Role>` (a session id maps to the role
its login resolved to). `auth::require_session` is joined by a new
`require_role(min: Role)` middleware, layered per route group in
`api::router` exactly the way `require_session` itself is layered today —
`viewer`-level routes get `require_session` alone, `operator`-level routes
get `require_session` then `require_role(Operator)`, `admin`-level routes
`require_role(Admin)`. A role check failing is `403`, distinct from
`require_session`'s `401`, so the frontend can tell "not logged in" from
"logged in, not allowed" and render accordingly.

### Audit trail: the controller learns *who*, not just *what*

Every accepted revision (config or intent) already answers "what changed and
when" via `GET /config/revisions`. It has never recorded **who** submitted
it — `gsp-controller` only ever sees `gsp-ui`'s single shared
`--controller-token`/`--aggregator-token`, identical for every human behind
it. Closing this gap needs one addition at each hop:

- `gsp-ui` sets a new `X-Actor: <username>` header on every write it
  proxies to the controller or aggregator (`aggregator_proxy`/
  `controller_proxy`, using `proxy_util`'s existing header-forwarding path
  in reverse — an outbound header added, not an inbound one preserved).
- `gsp-controller`'s `submit()`/intent `submit_intent()` read `X-Actor` (
  `Option<String>`, defaulting to `"unknown"` for a request that didn't
  carry one — e.g. a direct `curl` against the controller's own token,
  which stays valid break-glass access and simply produces a less specific
  audit entry) and store it in a new `actors` `sled` tree, keyed by
  revision number, alongside `stage` — same pattern, another small
  parallel tree rather than widening `Store`'s core content-agnostic
  contract.
- `GET /config/revisions` gains an `actor` field per entry. `gsp-aggregator`
  does the same for the fan-out verbs it forwards, logging `(instance,
  actor, verb, timestamp)` — in-memory only, matching its existing
  "carries no durable state" design; a durable fleet-wide *audit log*
  (as opposed to "the last actor value attached to a controller revision,
  which is already durable via `sled`") is not designed here — the
  controller's revision history already gives a durable, complete audit
  trail for every fact that has lasting effect (config, intent); the
  aggregator's own verb log is a convenience for "who just did this," not a
  compliance record.

This is intentionally a thin audit story, not a full authorization/audit
framework — proportionate to a three-role model with one human-facing
process, not a multi-tenant compliance product.

---

## The admin GUI

**Served only by the topmost `standalone` tier — never by a `slave` tier.**
Every regional controller/aggregator stays a machine-to-machine API (serving
its own subtree, relaying to its parent) with no browser-facing frontend at
all; only the root hosts the GUI, because it's the one place with full-fleet
authority and it keeps auth/RBAC/session state a single fact in a single
place, instead of N regional logins each needing to agree on who can do what.
Serving a UI per tier would reopen exactly the problem the hierarchy was
built to close: an operator not knowing *which* region's login is
authoritative for a given action, and RBAC that must be kept consistent
across every tier instead of living in one.

**A dedicated `gsp-ui` process is the GUI's home — not the controller, not
the aggregator.** The GUI needs both: reads/operational verbs from the
aggregator, config/revisions from the controller. Bolting human login onto
either one taxes it with a concern that isn't its own: a session store *is*
a form of authority ("who's allowed to act"), which sits wrong on the
aggregator (deliberately "carries no authority"); and putting the browser's
whole surface on the controller couples presentation to Tier-1's write path
for no reason. `gsp-ui` is a pure BFF — human session auth in front, holding
the controller's and aggregator's own machine credentials to call each on
the operator's behalf, authority over neither. It's the one piece of this
picture with no "real" state of its own (no store, no fleet data, nothing
that outlives a restart beyond active sessions), so it costs nothing to add
as a fourth (later: a third-per-tier) always-optional process; it never
gains its own write path around the controller.

A regional tier's raw API is still reachable directly (curl / CLI, on its own
internal network) as **break-glass** during a root outage or partition —
the same status the phase-5 direct-per-instance admin API already has next
to the aggregator/controller. That is deliberately *not* a served web UI:
break-glass access is for an operator who already knows exactly what they're
doing to one region, not a second, parallel product surface to build and
secure.

Two capability levels, shippable in order:

1. **Operational** — needs no Tier-1 store. Fleet-wide view + the phase-5
   verbs, via the stateless aggregator hierarchy. This is roadmap **Phase 10**
   and is the "Web UI for the admin API" listed in chapter 08's *Later /
   optional*. A first deployment is a single aggregator tier — the hierarchy
   above is what a deployment grows into with more regions, not a
   prerequisite for phase 10's first release.
2. **Full management** — structural config editing, revision history, rollback,
   diff, RBAC. Needs Tier 1 + the controller hierarchy. Roadmap **Phase 11**.

No proxy admin port is ever exposed to a human; the GUI carries all
authentication and authorization.

---

## Failure behaviour

| Component down | Data plane | Config changes | Health accuracy | Fleet view |
|----------------|-----------|----------------|-----------------|------------|
| Tier-1 store / all controllers | unaffected — serves last replica | frozen | unaffected | unaffected (aggregator is a separate path) |
| a `slave` controller tier | that subtree's proxies keep last replica | frozen for that subtree only; other subtrees unaffected | unaffected | unaffected |
| a `slave`/child aggregator tier | unaffected | unaffected | unaffected | that subtree's data ages out of the *parent's* view; the subtree's own local view is still fully served |
| Tier-2 fabric (a domain) | unaffected | unaffected | falls back to each instance's own checks | unaffected |
| a single proxy instance | LB / anycast routes around it | n/a | its Tier-2 records age out of the domain view | ages out of its tier's aggregator view |
| partition `domain ↔ parent controller` | the domain keeps running on its last replica | that domain frozen | intra-domain health still gossips | domain still fully self-servable via its local controller/aggregator |
| partition `domain ↔ parent aggregator` | unaffected | unaffected | unaffected | domain still fully self-servable locally; only the parent's rolled-up view is missing that domain |
| partition splitting a domain | both halves keep running | — | each half reaches quorum among itself; may diverge until healed (acceptable — health is advisory + locally checked) | each half's local aggregator/controller still serves that half |

---

## Security notes

- **Tier 1 is a high-value target** — a bad revision can blackhole the fleet.
  Mitigations: `validate()` on **both** controller and instance; a revision must
  be signed by the controller; an instance rejects a revision that fails a
  sanity bound (e.g. "a pool that had > 0 targets now has 0" → warn and keep the
  previous snapshot); staged / canary rollout; one-key rollback to any prior
  revision.
- **Tier 2 is lower value** (advisory, rebuildable) but still authenticated —
  signed messages / mTLS mesh.
- **The GUI is the only operator-facing component, and it is served solely by
  the root tier.** All authn / authz lives there, once, rather than being kept
  consistent across every regional tier's own frontend.

---

## Open questions

Resolved by this session's design pass (each now has its own section above,
plus an ADR in `docs/09`): **Tier-1 backing store / HA mechanism** (`sled` +
embedded `openraft`, one group per tier, ADR 21) — the "git-as-source-of-
truth" option was dropped once the concrete design confirmed `sled` already
does everything needed and a git-backed store would need its own
Raft-equivalent replication story anyway; **one revision stream or two**
(kept **two**, config and intent, confirmed by their independent slice 1/3/4
implementations and reaffirmed by "canary staging applies to config only" in
the rollout design — the two logs have different visibility and staging
needs, splitting was the right call); **adoption flow** (built, slice 5).

Still open:

- **Tier-2 transport**: an existing gossip crate (`foca` for SWIM, a
  `memberlist`-style mesh) vs. a minimal hand-rolled CRDT sync. Measure message
  volume with realistic backend / instance counts before committing.
- **Affinity as Tier-2 state**: should the resolver result cache / `sticky_key`
  become shared-within-a-domain state (so a rehashed client keeps its instance's
  affinity)? Overlaps chapter 03 scheme C and the deferred `sticky_key` design.
- **Region-scoped intent**: is "drain backend X" ever meant to apply to one
  domain only, needing intent records with a failure-domain selector? Also
  now a prerequisite for region-scoped RBAC (see "RBAC and audit" above,
  which explicitly deferred it for the same reason).
- **Controller ↔ discovery**: when phase-8 discovery already supplies backend
  membership from k8s / Consul / DNS, the controller's intent surface shrinks to
  *overrides* on top of discovery. Confirm the precedence order
  (discovery ∪ overlay − removed, then admin state) and where it is evaluated.
- **Aggregator push transport**: a bespoke small protocol (HTTP + a batched
  JSON body, simplest, consistent with the rest of the admin API) vs. an
  existing wire format (OTLP for metrics-shaped data) — the OTLP path buys
  interop with existing collectors but the fleet/pool/session view isn't
  metrics-shaped, so a bespoke shape is likely still needed alongside it.
- **Push buffering bound**: how much history a proxy/aggregator holds locally
  when its parent link is down before it starts dropping — a fixed ring
  buffer sized in the same spirit as the existing recv-buffer caps
  (`docs/06`), not unbounded growth.
- **Dynamic Raft membership** (add/remove a controller replica in a running
  HA group without a coordinated restart): explicitly deferred in "Intra-tier
  HA (design)" above — `openraft` supports it, this design just doesn't use
  that support yet.
- **A durable, fleet-wide audit *log*** (as opposed to the per-revision
  `actor` field this session's RBAC design adds, which is durable but lives
  one field per revision, not as its own queryable log): deferred in "RBAC
  and audit" above as disproportionate to a three-role, single-human-facing-
  process model.
