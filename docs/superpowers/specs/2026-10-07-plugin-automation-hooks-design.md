# Plugin automation hooks (addendum to the plugin system design)

Status: design for review (brainstormed 2026-10-07 with Leandro). Decisions are marked **[decided]**; the rest is a recommendation the plugin build plan follows unless the maintainer changes it. No code in this wave.

This extends the [plugin system design](2026-10-07-plugin-system-design.md) (#221), which stays valid except where this document says otherwise. It settles the items that spec left open: what a plugin is for, what wakes it, the route model, how module bytes replicate, and secret storage.

**Supersedes in #221:** the guest exports (`tick` becomes `on_timer` plus the other triggers), `routes_replace(set)` (replaced by the `routes` capability and its entries), the open item on module-byte replication, and the secrets paragraph (encryption at rest, details gated on #235). New in this document: the `backends` capability, webhooks and events. Everything else in #221 holds. Where the two disagree, this document wins.

## Purpose

**[decided]** A plugin is a general automation hook, not only a route syncer. It reacts to something (a timer, a Wayhouse event, a call from an outside system) and may then act through a fixed set of capabilities the operator approved. Keeping routing in sync with an outside system (the Pelican panel, #213) is the first use, not the limit. Other expected uses are alerting, and automation tools such as Ansible calling in through a webhook.

This changes one sentence of #221: the guest no longer exports only `init` and `tick`. It exports `init` plus the trigger handlers below, and the capability list is the extension point for everything a plugin can do.

## Trust model

**[decided]** Two layers, with different jobs:

- **Hard wall, every plugin:** the WASM sandbox, the limits, and the approved capabilities. A plugin, official or not, can do only what the operator approved against its sha256.
- **Review, official repo only:** plugins in `wayhouse-proxy/plugins` are reviewed and tested, so an operator can trust that they ask for no more than they need. External registries get no review and are installed at the operator's own risk, shown as such in the approval screen.

Review is never the only protection: reviews miss things, and external repos get none.

## Triggers

All three share one rule: **one call per plugin at a time**, run on the HA leader, with the same memory, time and call budgets as a tick in #221. A trigger that arrives while the plugin is busy is queued up to a small cap. **One queue slot is reserved for `on_timer`**, so a webhook or event flood cannot starve the reconcile. A full queue is dropped and counted.

**Commit rule.** All three triggers commit through the same single entry as #221's tick: the `state_put`, `routes` and `backends` effects of one call are one replicated entry tagged with the leader term and the state revision the call read, compare-and-set on apply. A stale leader's entry is rejected on apply. A call whose commit is rejected, or that cannot commit (leader change, no quorum), returns **503 with a retry hint** to a webhook caller; a full queue returns **429**. A webhook never returns success for state that was not committed. Events and timers that fail to commit are retried by the next timer reconcile.

- **`on_timer`** runs on the manifest `tick_interval` (host-enforced minimum, 10 s). This replaces `tick` from #221.
- **`on_event(kind, payload)`** receives a Wayhouse event the plugin declared in its manifest (and had approved). The event list is published, versioned and **only grows**; payloads only gain optional fields. The starter set is limited to what the controller itself observes: config revision applied, backend health changed (as seen through the aggregator and gossip), plugin installed or changed (name and version only, never another plugin's config or capabilities). Events the controller cannot see (per-connection data) are out of scope; that is the data path.
- **`on_webhook(request) -> response`** is served at `POST /plugins/<install id>/hook` on the controller. Each plugin install gets its own webhook token, shown once to the operator and stored hashed. Every other controller route sits behind the admin bearer layer, so this endpoint is an **explicit carve-out** with its own scheme:

- It is served by a **separate, optional listener that is off by default** (its own bind address and TLS), never on the admin port, so exposing webhooks to an automation host does not expose the admin API.
- The token is checked **before the body is read**, in constant time, after a **per-source rate limit** that runs before the compare (so the token cannot be brute-forced at line rate). There is also a body-size cap and a per-install rate limit.
- The token hash and the enabled flag are part of the replicated install record, so a follower can verify before forwarding. The endpoint is off until the operator enables it for the install, and the token can be rotated and revoked.
- A follower authenticates the request itself and forwards it to the leader over the existing authenticated peer channel; it never forwards an unauthenticated request.
- Webhooks and events run on the **root tier only** (a slave-tier controller does not serve the hook), as in #221.
- The response headers a guest may set are an allowlist (content type and custom `x-` headers); no `Set-Cookie`, hop-by-hop or framing headers. The guest sees the method, path suffix, query, headers (without the token) and body, and returns a status, headers and body of capped size.

A plugin declares the triggers it uses; an undeclared trigger is never delivered. The set of declared triggers is part of the approved capabilities, so adding one in an update needs approval again.

## Capabilities (host imports)

**[decided]** Every effect is its own capability, listed on the approval screen in plain words and bound to the module sha256 (as in #221). A call without the capability traps the call, not the controller.

| Capability | What it allows | Limits |
|------------|----------------|--------|
| `http` | HTTPS requests to approved hosts, secrets expanded by the host | #221 network rules, per-call budgets |
| `state` | private key-value store | per-plugin total size cap and write-rate cap (a webhook can trigger writes, and state is replicated), replicated |
| `routes` | declare **entries** `{hostname pattern, backend address}` (see below) | allowed hostname patterns, backend constraints, count cap |
| `backends` | add, remove or patch backends in pools the operator named | named pools only, op-rate cap, ownership rules below |
| `log` | log lines tagged with the plugin | rate limit |

New capabilities are added the same way: one import, one approval line, its own limits. Nothing outside this table is reachable. Network access beyond `http`, launching processes, reading other plugins' state and touching operator routes are not offered.

### Backends capability: ownership and fencing

The existing intent ops (`BackendAdd`, `BackendRemove`, `BackendPatch` in `crates/wayhouse-controller/src/intent.rs`) carry only a pool and an address: no actor, no owner, no compare-and-set, and they are their own log entries. They cannot be handed to a plugin as they are. **[decided]** Plugin backend ops are tagged with the install id; a plugin may only remove or patch backends it added itself (never an operator's); and they commit in the same term-checked entry as the call's other effects, not as separate intent entries. This is a **precondition** for building `backends`, which stays last in the phasing.

### Route model

**[decided]** `routes_replace` takes narrow entries, not the core `Route` type. The controller turns a plugin's entries into plugin-owned pools and routes, ordered after operator routes (operator routes always win). Reasons: the controller has no routes API today (routes are structural config, matcher to pool; the operator-intent log only edits backends of existing pools and route hints), and exposing the core type would tie the plugin ABI to core's route model.

- Ownership is `plugin:<install id>`; a plugin replaces only its own set, as one declarative call per trigger.
- The generated pools and routes are visible in the admin API and UI, marked with their plugin, so operators can see where a route came from.
- The entry shape is `{host: "<pattern>", backend: "<ip:port>"}`. It does not yet say which listener or protocol (TCP, UDP, sniffer) the route attaches to; that is a named input of the `routes` step and is tracked in #236 before that step starts.
- Two plugins claiming the same host pattern: the earlier install wins, deterministically, and the conflict is shown in the admin API and UI; the later claim is not applied. The exact controller-side materialisation (how generated routes are merged into the resolved config on every instance) is the main design task of the build plan and is tracked as its own issue.

## Module bytes in HA

**[decided]** The replicated log carries only an install record: plugin name, sha256, size, approved capabilities, config, webhook token hash and enabled flag. The module bytes are a content-addressed blob kept out of the Raft log and snapshots (up to 8 MiB per plugin).

- **Upload goes to the leader**, which holds the blob and only commits the install record once the blob is on a quorum of nodes (peers fetch it and acknowledge). An install therefore cannot be stranded on a node that dies right after upload.
- Every controller fetches a missing blob from the leader or any peer and verifies the sha256 before use. An install becomes active on a node only once that node holds the blob; until then the node reports "pending" in the plugin status. A node that becomes leader without a blob it needs raises a visible alert instead of silently running nothing.
- The peer fetch endpoint is a new peer-protocol surface: it needs peer authentication (as the Raft routes have) and is gated for rolling upgrades like any new entry type (the #221 gating rule).
- A module is garbage-collected only when no install references it **and** it is not the previous version kept for rollback (#184) until the update is confirmed.

## Secrets

**[decided] direction, details to review.** Secrets are encrypted at rest with **one cluster key, provisioned identically to every node out of band** (file path or environment). The key is never written to the replicated log or snapshots, which hold only ciphertext, so any node can decrypt what the leader wrote. A secret is never returned by an API, logged, or passed to the guest; the host expands it into the request, and only for the host the slot is bound to (#221).

This stays a **hard gate**: before any plugin that uses secrets ships, a security review must decide key rotation, how a node joining the cluster gets the key, what happens when a node lacks it (the plugin is held, not run without its secret), and log compaction so a superseded ciphertext does not linger in old segments. Tracked as its own issue.

## Pelican (#213)

The Pelican plugin is the first plugin and the design test. Verified from Pelican's source on 2026-10-07 (the primary docs site `pelican.dev` is blocked from the build environment):

- The Application API (`routes/api-application.php`) has `GET /api/application/servers`, `/nodes`, `/nodes/{id}/allocations`, `/nodes/{id}/configuration`, server `transfer`, and webhook management under `/api/application/webhooks` (with `types` and `events` listings).
- API keys are per resource (`Server, Node, Allocation, ...`) with permission levels 0 to 3, an IP allowlist and an optional expiry (`app/Models/ApiKey.php`), so a key limited to read access on servers, nodes and allocations looks possible. Which level number means "read" is not confirmed.
- Pelican has its own "plugins" (hub.pelican.dev). Docs must say "Wayhouse plugin" to avoid confusion.

Still **unverified** and to be checked against a real panel before the plugin is built: the server response fields (primary allocation, node), whether any hostname or subdomain exists per server, the webhook event names and payloads for server create, delete and transfer, and which allocation is the public one when a server has several.

The plan fits either way. With webhooks, the Pelican plugin uses `on_webhook` for create, delete and transfer events and `on_timer` as a slow reconcile. Without usable webhooks it polls on `on_timer`. Both end in one `routes` call.

## Non-goals

Plugins on the proxy data path (sniffers); plugin-to-plugin calls; launching containers or processes; arbitrary network access; a general-purpose scripting runtime; a plugin SDK beyond the ABI crate (a Rust helper crate is likely, decided in the plan).

## Risks and open questions

- **A broader surface needs discipline.** Each capability, event and webhook field is public API once released. The starter sets are deliberately small and additive.
- **Webhook endpoint.** A new authenticated network surface on the controller. Needs rate limits, token handling and abuse tests in the plan.
- **Event delivery on a leader change.** Events during an election may be missed. Plugins must treat events as hints and reconcile on `on_timer`; the docs say so.
- **A malicious plugin within its approved capabilities** can still misuse them (for example, route to an attacker's address if the `routes` backend constraint is loose). The approval screen shows constraints plainly, and the official repo's review checks that requested constraints are tight.
- **Pelican facts** above remain unverified until checked on a real panel.
- **Controller-side route materialisation** (see Route model) may force a change to how the resolved config is built; if it does, it is a core change with its own spec.

## Phasing

Unchanged from #221: Wave 5 builds the ABI crate and conformance harness, the controller plugin host, the plugin API, secret storage (after its review), the UI install and approval pages, the plugins repo bootstrap and registry `kind = plugin`. Within it, order: `on_timer` + `http` + `state` + `log` first, then `routes`, then `on_webhook`, then `on_event` and `backends`.
