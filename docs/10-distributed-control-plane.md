# 10 – Distributed control plane (v2)

## Status

**Design only. Nothing here is built.** Targets roadmap phases 10–12
([08-roadmap.md](08-roadmap.md)). The v1 data plane and the single-instance
control plane (chapters [02](02-architecture.md), [05](05-configuration.md),
[06](06-operations-observability.md)) are unchanged by this: every mechanism
below is either an **additional writer** feeding the same validated `Snapshot`
swap, or an **additional input** to the same per-backend health flag. The hot
path gains no network call and no lock.

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

## The controller (`gsp-controller`)

A **new, optional service**. Proxies never talk to each other through it — it is
the Tier-1 writer, the fleet's read aggregator, and the GUI's only backend.

- **Owns the Tier-1 store** — embedded (Raft in the controller), or backed by
  etcd / a SQL DB / a git repo. This is an ADR-level choice, deferred (see Open
  questions).
- **Config API.** Submit a full or partial config; the controller runs the
  **same `validate()`** the proxy does, assigns a revision, publishes. History,
  diff, one-key rollback, staged / canary rollout (a subset of instances take a
  revision first).
- **Intent API.** The phase-5 admin verbs (`drain backend`, add / remove
  backend, `route-hint`, …) but fleet-wide and persisted — each writes a Tier-1
  revision instead of poking one instance. Direct per-instance admin stays as
  break-glass.
- **Read aggregation.** Fans `GET /config`, `/pools`, `/metrics`, `/healthz` out
  to all instances (or scrapes them) and merges — the single fleet view.
- **Auth / RBAC / audit log live here.** The proxies' own admin APIs stay
  internal-only and are locked to the controller's identity (or its network).
- **HA.** Run N controllers behind a leader lock for *writes*; reads and the
  Tier-1 fan-out are stateless. A controller outage stops *changes*, not the
  data plane — instances serve their replicas.

---

## The admin GUI

A pure client of the controller. Two capability levels, shippable in order:

1. **Operational** — needs no Tier-1 store. Fleet-wide view + the phase-5 verbs,
   via a thin stateless aggregator. This is roadmap **Phase 10** and is the
   "Web UI for the admin API" listed in chapter 08's *Later / optional*.
2. **Full management** — structural config editing, revision history, rollback,
   diff, RBAC. Needs Tier 1 + the controller. Roadmap **Phase 11**.

No proxy admin port is ever exposed to a human; the GUI carries all
authentication and authorization.

---

## Failure behaviour

| Component down | Data plane | Config changes | Health accuracy |
|----------------|-----------|----------------|-----------------|
| Tier-1 store / all controllers | unaffected — serves last replica | frozen | unaffected |
| Tier-2 fabric (a domain) | unaffected | unaffected | falls back to each instance's own checks |
| a single proxy instance | LB / anycast routes around it | n/a | its Tier-2 records age out of the domain view |
| partition `domain ↔ controller` | the domain keeps running on its last replica | that domain frozen | intra-domain health still gossips |
| partition splitting a domain | both halves keep running | — | each half reaches quorum among itself; may diverge until healed (acceptable — health is advisory + locally checked) |

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
- **The GUI is the only operator-facing component.** All authn / authz lives in
  the controller behind it.

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
