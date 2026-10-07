# Plugin system design (integrations, #213)

Status: design for review (brainstormed 2026-10-07 with Leandro). Decisions are marked **[decided]**; the rest is a recommendation the future implementation plan follows unless the maintainer changes it. No code in this wave.

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
- Guest exports: `memory`, `alloc`, `init(config_ptr, len)`, `tick()`; optional `on_event(ptr, len)`. The host drives the guest on a manifest-declared interval (minimum enforced by the host, e.g. 10 s). The guest never runs its own loop or threads.
- Host imports (all capability-gated; a call without the capability traps the call, not the controller):
  - `http(request) -> response`: HTTPS only, destinations restricted to `net.hosts` patterns from the approved manifest, body/time/call-count budgets, the same resolver-level SSRF guard as the registry client. Header values may contain `${secret:NAME}`; the **host expands it, so the guest never sees the secret**.
  - `state_get/state_put`: private key-value store per plugin, size-capped, replicated.
  - `routes_replace(set)`: declarative sync of the routes the plugin owns. A plugin can only create, change and delete entries in its own namespace (`source = plugin:<name>`) and cannot touch operator-defined routes.
  - `log(level, msg)`: rate-limited, tagged with the plugin name.
- Limits as for sniffers: memory cap, epoch timeout per call, fresh `Store` per call; additionally a per-tick HTTP budget.

## Manifest and registry

- Per-plugin `manifest.toml` in the plugins repo: `name`, `description`, `license`, `version`, `plugin_abi`, `min_controller`, `limits`, `tick_interval`, `capabilities` (`net.hosts`, `secrets` slot names with descriptions, `routes.namespace`, `state` size), `config` schema (non-secret settings).
- The registry `index.json` is the sniffer format with a top-level `"kind": "plugin"` and a `plugins` list whose version entries add `capabilities`. The crate parses `kind` now and accepts only `sniffer`; adding `plugin` is part of the plugin build wave. `schema` stays 1 (nothing is released).
- Official plugins are the small tested set from #219; community plugins are tested by the plugins repo's own CI.

## Capabilities and trust

- At install the UI shows the requested capabilities in plain words (hosts it may call, secret slots it needs, route namespace) and the registry's trust state (official signed / unsigned / external at own risk). The operator approves; the approved set is stored with the install.
- On update, any capability not already approved needs approval again. An update never silently widens access.
- Secrets are entered by the operator per plugin instance, stored in the controller, never returned by any API, never logged, never passed to the guest.
- Admin-authenticated like every other mutating route; no new auth scheme.

## HA and state

- Plugin installs, approved capabilities, config, secrets and plugin state are part of the replicated controller state.
- A plugin ticks on the HA leader only; on leadership change the new leader starts it from the replicated state. `routes_replace` goes through the same replicated write path as other route changes, so a follower never writes.
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
- A malicious plugin within its approved hosts can still exfiltrate data it can read (route data, its own state); capabilities bound this, they do not remove it. The UI wording must say so.
- Budget and fairness: a slow plugin must not stall the leader's other work; ticks run on a bounded worker pool.
