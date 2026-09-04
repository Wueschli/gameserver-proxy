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
  data plane — every instance under it serves its last replica.

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

- **Tier-1 backing store**: embed Raft in the controller vs. lean on etcd vs.
  git-as-source-of-truth. Drives the HA story and operational familiarity.
- **One revision stream or two?** Structural config and operator intent could
  share a single ordered stream (simpler to reason about) or be split (intent
  moves faster, with looser durability requirements).
- **Tier-2 transport**: an existing gossip crate (`foca` for SWIM, a
  `memberlist`-style mesh) vs. a minimal hand-rolled CRDT sync. Measure message
  volume with realistic backend / instance counts before committing.
- **Affinity as Tier-2 state**: should the resolver result cache / `sticky_key`
  become shared-within-a-domain state (so a rehashed client keeps its instance's
  affinity)? Overlaps chapter 03 scheme C and the deferred `sticky_key` design.
- **Region-scoped intent**: is "drain backend X" ever meant to apply to one
  domain only, needing intent records with a failure-domain selector?
- **Controller ↔ discovery**: when phase-8 discovery already supplies backend
  membership from k8s / Consul / DNS, the controller's intent surface shrinks to
  *overrides* on top of discovery. Confirm the precedence order
  (discovery ∪ overlay − removed, then admin state) and where it is evaluated.
- **Adoption flow** (a `standalone` tier becoming a `slave` post-install, via
  the admin UI): revision-history reconciliation and a quiescence
  precondition before the role flip — see "Adoption" above. Not needed for
  phases 10/11's first release; pick this up when a real multi-region
  deployment needs it.
- **Aggregator push transport**: a bespoke small protocol (HTTP + a batched
  JSON body, simplest, consistent with the rest of the admin API) vs. an
  existing wire format (OTLP for metrics-shaped data) — the OTLP path buys
  interop with existing collectors but the fleet/pool/session view isn't
  metrics-shaped, so a bespoke shape is likely still needed alongside it.
- **Push buffering bound**: how much history a proxy/aggregator holds locally
  when its parent link is down before it starts dropping — a fixed ring
  buffer sized in the same spirit as the existing recv-buffer caps
  (`docs/06`), not unbounded growth.
