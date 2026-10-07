# Plugin system design (integrations, #213)

Status: design for review (brainstormed 2026-10-07 with Leandro). Decisions are marked **[decided]**; the rest is a recommendation the future implementation plan follows unless the maintainer changes it. No code in this wave.

**Partly superseded** by the [automation hooks addendum](2026-10-07-plugin-automation-hooks-design.md) for: guest exports, `routes_replace`, module-byte replication and secrets storage. Implement from the addendum where the two differ.

## Vocabulary

- **Sniffer**: WASM protocol/hostname module on the proxy data path. Spec: [sniffer registry](2026-10-05-plugin-registry-design.md). Repo: `wayhouse-proxy/sniffers`.
- **Plugin**: integration with an outside system (first target: the Pelican panel, #213) that runs in the controller and manages routes. Repo: `wayhouse-proxy/plugins`. This document.

## Goal

Let anyone write a plugin, let operators install it from a registry and enable it like a sniffer (browse, verify, one click, sandboxed), and let it work without the UI.

## Decisions

- **[decided] Runtime: WASM with capability-gated host imports.** A plugin is a core WASM module like a sniffer, with its own ABI (separate crate and version section from the sniffer ABI). Sidecar processes and built-in integrations were rejected: a sidecar cannot be installed like a sniffer, and built-in integrations leave no room for community plugins.
- **[decided] Host: the controller.** Plugins manage proxies, so they live in the control plane and keep working when the UI is down. The UI is only a client of the controller's plugin API (install, configure, status).
- **[decided] Install flow mirrors sniffers.** The registry client (fetch, sha256, optional minisign) stays in the UI backend; it uploads the module to the controller, which re-validates it and is the authority. Trust model is the sniffer one (official registry, external registries at own risk, unsigned allowed in the first cut) plus capability approval.
- **[decided] Build is a later wave.** Wave 3 only ships the sniffer registry; the registry index carries a `kind` field so the plugins repo can reuse the format.

## Plugin ABI (sketch)

- Custom section `wayhouse.plugin-abi` (major, minor), exact minor match while major is 0, checked before instantiation, same policy as the sniffer ABI.
- Guest exports: `memory`, `alloc`, `init(config_ptr, len)`, `tick()`. The host drives the guest on a manifest-declared interval (minimum enforced by the host, e.g. 10 s). The guest never runs its own loop or threads.
- Host imports (all capability-gated; a call without the capability traps the call, not the controller):
  - `http(request) -> response`: HTTPS only, destinations restricted to the approved `net.hosts`, body/time/call-count budgets. Header values may contain `${secret:NAME}`; the **host expands it, so the guest never sees the secret**, and only when the destination is a host that slot is bound to (see Secrets). Private destinations: see Network access below.
  - `state_get/state_put`: private key-value store per plugin, size-capped, replicated.
  - `routes_replace(set)`: declarative sync of the routes the plugin owns. A plugin can only create, change and delete entries it owns (`source = plugin:<install id>`, see Install identity) and cannot touch operator-defined routes. Ownership alone does not bound what an entry points at, so see Route limits below.
  - `log(level, msg)`: rate-limited, tagged with the plugin name.
- Limits as for sniffers: memory cap, epoch timeout per call, fresh `Store` per call; additionally a per-tick HTTP budget.
- The controller holds secrets and the store, so a module is also bounded at compile time: size cap (8 MiB as for sniffers), a compile timeout, and wasmtime limits on function count, locals and table size; compilation runs off the leader's main loop on a bounded pool. A wasmtime bug or compile-time DoS matters more here than on a proxy; this is listed under Risks.

## Manifest and registry

- Capabilities are **embedded in the module** (custom section `wayhouse.plugin-caps`, read without instantiating) and bound to its sha256. The index's `capabilities` copy is informational for browsing only; the controller reads the section from the downloaded bytes, rejects a module whose section differs from the index, and shows the section's content in the approval screen. An unsigned or external registry therefore cannot show one thing and install another.
- Per-plugin `manifest.toml` in the plugins repo: `name`, `description`, `license`, `version`, `plugin_abi`, `min_controller`, `limits`, `tick_interval`, `capabilities` (`net.hosts`, `secrets` slot names with descriptions, `routes.namespace`, `state` size), `config` schema (non-secret settings).
- The registry `index.json` is the sniffer format with a top-level `"kind": "plugin"` and a `plugins` list whose version entries add `capabilities`. The crate parses `kind` now and accepts only `sniffer`; adding `plugin` is part of the plugin build wave. `schema` stays 1 (nothing is released).
- Official plugins are the small tested set from #219; community plugins are tested by the plugins repo's own CI.

## Capabilities and trust

- At install the UI shows the requested capabilities in plain words (hosts it may call, secret slots and which hosts they are bound to, hostname patterns and backend constraints for routes, route-count cap) and the registry's trust state (official signed / unsigned / external at own risk). The operator approves; **approval is recorded against the module's sha256 plus the capability set**.
- On update (a new sha256), any capability not already approved needs approval again. An update never silently widens access.
- **Secrets.** Entered by the operator per plugin instance, stored in the controller, never returned by any API, never logged, never passed to the guest, and redacted from any response or log line that would echo a request or response. Each secret slot is **bound to specific hosts** in the manifest and approved as such, so a token cannot be sent to a second approved host. **Hard gate:** secrets in the replicated log and snapshots persist in old log segments and backups, so the secret storage review (at-rest encryption, key management, log compaction) must be finished before any plugin that uses secrets ships.

### Network access

The `http` import does not reuse the registry client's guard unchanged, because a Pelican panel is often on a LAN address or the same host. Rules: public destinations are allowed once the host is approved; **private, loopback and link-local destinations need explicit per-host operator approval** (shown as such in the approval screen, default off); the resolved IP is pinned at connect time and checked on every connection (stops DNS rebinding); redirects are followed only within the approved hosts and never to a private address that was not approved.

### Route limits

`routes_replace` is bounded by approved capabilities: allowed hostname patterns (a plugin can only claim hostnames matching them), constraints on backend addresses (patterns or CIDRs), and a route-count cap. **Operator routes always win** on a conflict. The approval screen shows all of these.

### Install identity and uninstall

Each install gets an install id, and route ownership is `plugin:<install id>`, so two registries shipping the same plugin name do not collide. Disabling or uninstalling a plugin removes its routes and state after a short grace period configured per install (default: routes removed immediately on uninstall, kept as last-known-good for a bounded time, default 10 minutes, when the plugin is disabled or its ticks keep failing, then removed with a visible alert).
- Admin-authenticated like every other mutating route; no new auth scheme.

## HA and state

- Plugin installs, approved capabilities, config, secrets and plugin state are part of the replicated controller state.
- A plugin ticks on the HA leader only. Raft rejects a deposed leader's proposals, but a plugin's HTTP calls and in-memory state are not fenced, so the tick result is committed as **one replicated entry at the end of the tick** holding `state_put` and `routes_replace` together, tagged with the **leader term and the state revision the tick read** (compare-and-set on apply). A stale leader's entry is rejected on apply. If there is no quorum the tick is skipped. A new leader waits a grace period before its first tick (the tunnel sweeper in `docs/11-backend-transport.md` does the same, and its `Expire` entry re-checks on apply).
- **Rolling upgrades (N and N-1).** Plugin entry types are gated until every replica supports them, following the gating rule in the [component versioning spec](2026-10-05-component-versioning-design.md); an N-1 leader would also refuse a module with a newer plugin ABI (exact minor match while major is 0).
- **Slave tiers.** A `--role slave` controller receives its writes from the parent by relay, so plugins run on the **root tier only** in the first cut; a plugin in a slave tier would conflict with relayed config.
- Open: how module bytes (up to 8 MiB) are replicated (store them in the replicated state, or each node fetches by sha256 from the leader).

## Pelican as the first plugin (#213)

Poll the Application API on `tick` with a read-only token in a secret slot, map servers' primary allocations to node and hostname, `routes_replace`. Whether Pelican exposes webhooks, a hostname per server and a read-only token scope is still unverified (#213 acceptance: check a real panel). The plugin spec stays valid either way; only the plugin changes.

## Non-goals

Plugins on the proxy data path (that is what sniffers are); plugin-to-plugin calls; launching containers; arbitrary outbound network access; a plugin SDK beyond the ABI crate (a Rust helper crate is likely, decided in the plan).

## Phasing

1. Wave 3 (now): sniffer registry, install, repo move, updates, with the `kind` field.
2. Wave 5 (new, after Wave 4): plugin ABI crate and conformance harness, controller plugin host (leader only), controller plugin API, secret storage, UI install and approval pages, plugins repo bootstrap, registry `kind = plugin`. Then the Pelican plugin once #213 is verified.
3. #219 (CI for both repos) lands with the repo bootstraps.

## Risks and open questions

- Secret storage in the replicated store (at-rest encryption, key management): needs its own review before the build.
- The route model the host exposes (`routes_replace` shape) must be checked against the controller's intent and route types when planning.
- The controller hosts wasmtime and the secrets store: a compile-time or memory DoS on a module, or a wasmtime bug, is a control-plane problem. Mitigated by the compile limits above and by running compilation on a bounded pool; needs its own fuzz/limit tests in the plan.
- A malicious plugin within its approved hosts can still exfiltrate data it can read (route data, its own state); capabilities bound this, they do not remove it. The UI wording must say so.
- Budget and fairness: a slow plugin must not stall the leader's other work; ticks run on a bounded worker pool.
