# Sniffer registry, install and update design (#183, #184)

> **Terminology update (2026-10-07).** Leandro split the old "plugin" concept in two. **Sniffers** are the WASM protocol/hostname sniffer modules and live in [`wayhouse-proxy/sniffers`](https://github.com/wayhouse-proxy/sniffers). **Plugins** are integrations with other systems (e.g. the Pelican panel, #213) and live in [`wayhouse-proxy/plugins`](https://github.com/wayhouse-proxy/plugins); their design is still open. Wherever this document says "plugin" or "plugins repo" for a WASM sniffer module, read **sniffer** / **sniffers repo**. Code identifiers, crates and paths were renamed to "sniffer" in the same PR as this banner; older text below may still show the old names.


Status: design for review (brainstormed 2026-10-05). Decisions from the maintainer are marked **[decided]**; everything else is a recommendation the implementation plans follow unless the maintainer changes it.

## Goal

Let an operator install and update trusted sniffers from the web UI: an official registry (`wayhouse-proxy/sniffers`) of community sniffers, plus external registries at the operator's own risk, with trust checks regardless.

## Non-goals

Sandboxing changes (the WASM sandbox stays: no imports, memory cap, call timeout); sniffer hot-update across a fleet in lock-step (rollout is per node pull through the existing fleet fan-out); a package manager with dependency resolution (sniffers are single `.wasm` files).

## Decisions already made

- **[decided]** All bundled sniffers move to the sniffers repo; main keeps no sniffers, only the ABI crate and the conformance harness. Main's own tests use tiny WAT modules; e2e tests and the images that need real sniffers fetch a pinned sniffers release by tag and sha256 (`sniffers.lock`).
- **[decided]** Signatures: minisign for the official repo, **optional in the first cut** (verified when present). Sigstore deferred.
- **[decided]** Update discovery is on demand from the UI only (no background polling).
- **[decided]** Sniffers repo is bootstrapped after v0.1.0. ABI version lands before v0.1.0.
- **[decided]** Pinned-only updates by default.
- **[decided 2026-10-05, by decision card in the project thread; the transition plan has not been updated to list it]** The registry client (download, verification) lives in the UI backend, not on each proxy. Alternative considered: each proxy fetches itself (no UI dependency, but every edge proxy needs internet egress and its own trust config).
- **[settled in brainstorming]** Pinned instances get a copy-paste pin snippet in the UI; automatic config edits are out of scope for the first cut.

## ABI version

Custom section `wayhouse.abi` (major u16 LE, minor u16 LE), read by the host without instantiating. Starts at `0.1`; while the major is 0 the host requires an exact match. Plan: `2026-10-05-plugin-abi-version.md`.

## Registry format

A registry is a static HTTPS location serving `index.json` (the same format for the official repo and external ones) and the artifacts it references.

```json
{
  "schema": 1,
  "name": "wayhouse official sniffers",
  "sniffers": [
    {
      "name": "minecraft",
      "description": "Minecraft handshake virtual hosts",
      "license": "MIT OR Apache-2.0",
      "homepage": "https://github.com/wayhouse-proxy/sniffers/tree/main/plugins/minecraft",
      "versions": [
        {
          "version": "0.1.0",
          "abi": "0.1",
          "min_proxy": "0.1.0",
          "url": "https://github.com/wayhouse-proxy/sniffers/releases/download/minecraft-v0.1.0/minecraft.wasm",
          "sha256": "<64 hex>",
          "size": 24576,
          "signature_url": "<url of minecraft.wasm.minisig>",
          "limits": { "max_memory_bytes": 1048576, "call_timeout_ms": 50 },
          "config": "optional documentation of the per-module config string"
        }
      ]
    }
  ]
}
```

Per-plugin source manifest (`plugins/<name>/manifest.toml` in the sniffers repo) holds `name`, `description`, `license`, `version`, `abi`, `min_proxy`, `limits`, `config`; CI turns manifests plus built artifacts into `index.json` (sha256, size, urls) so nobody edits hashes by hand. Rules: `name` matches `[A-Za-z0-9_-]+` (the loader's module name rule), `version` is SemVer, `url` must be `https`, `sha256` is lowercase hex, `size` at most `MAX_MODULE_BYTES` (8 MiB, introduced by the upload-validation plan for #171; it does not exist on main yet), versions sorted newest first by the generator.

## Where the client lives (differs from the transition plan; decided)

The transition plan put `POST /admin/sniffers/install` on every proxy. Recommendation: the **fetch and verify step lives once in the UI backend** (`wayhouse-ui`, which already holds `reqwest`, the aggregator proxy and the fleet upload fan-out `POST /api/fleet/sniffers`), and the proxies keep only the upload endpoint they already have. Reasons: proxies need no outbound internet access (they are the DDoS-exposed edge), trust decisions are made in one place, and the per-node validator from #171 still re-checks every module that arrives. Trade-off: the UI backend needs egress to registries. The install flow:

1. UI backend loads the registry index (cached in memory for 5 minutes, bounded size 1 MiB, `https` only, redirects to `https` only, 10 s timeout).
2. Operator picks a sniffer and version; the backend checks `abi` against its own `HOST_ABI` constant (copied at build time, tested equal to the proxy's) and `min_proxy` against the fleet's reported `wayhouse_build_info` versions, refuses on mismatch with the reason.
3. Backend downloads `url` (size cap from the index and `MAX_MODULE_BYTES`), verifies sha256, verifies the minisign signature when `signature_url` is present, runs the shared module checker (the #171 validator moved into a small shared function, or a re-implementation using `wasmparser` for the section and imports; the proxies stay the authority).
4. Backend calls the existing fleet upload fan-out and reports per-instance results.
5. **Pinned instances are protected by the proxy, not by the UI.** An instance with `settings.sniffers.modules` set fails the whole scan on an unpinned or hash-mismatched file, and startup is fatal on that (`main.rs`, `build_sniffers(...)?`); a stray file would therefore stop the next restart. So the proxy's upload handler (and rollback) checks the pins first: with pins configured, a module is written only if its name is pinned and its sha256 equals the pin, otherwise `409` with the expected pin and **nothing is written**. The fan-out reports that per instance and the UI shows the exact pin line (`name`, `sha256`) to add to the config; after the config change the operator installs again. This also closes the same hole for manual uploads. Implemented in the upload-validation plan (Task 3), consumed by the install and update plans.
6. `min_proxy` needs each node's version, which the aggregator does not have today (`IngestPayload` carries no version; build info lives on each component's own `/metrics`). Until the component-upgrades plan adds `version` to the ingest payload (after v0.1.0), the check is skipped with a visible note in the UI.

## Trust

- Hash from the index always verified. The sha256 comes from the same index as the URL, so for an **unsigned** sniffer it protects against corruption only, not against a compromised registry location; the only authenticity beyond TLS is the minisign signature. The UI therefore shows "unsigned" prominently for every unsigned sniffer, official or not.
- Minisign: the official key's public half is compiled into the UI backend (`registry_keys.rs`); verification via the `minisign-verify` crate (verification only, no signing code). An official-registry sniffer shows "signed by wayhouse" or "unsigned"; unsigned is allowed in the first cut (decided) and becomes an opt-out later. **Manual step for the maintainer:** generate the key pair offline and publish the public key; the secret key never enters a thread or CI log (signing in plugins-repo CI uses a repository secret).
- External registries: added by URL in the UI, persisted by the UI backend (a JSON file in its data dir; one path flag `--registries-file`), always shown with an "at your own risk" warning on add and on every install, plus the sniffer's limits and config string. No signature requirement for external repositories.
- The official URL ships as a default registry entry, removable.
- Install and update are admin-authenticated like every other mutating UI route (existing bearer-token mechanism; do not add a new auth scheme).

## Updates (#184)

- Discovery is a button: the UI backend lists each instance's installed modules (`GET /api/fleet/instances/{i}/sniffers`, includes `sha256`), maps each sha256 to a version in the loaded indexes, and marks "update available" when a newer compatible version exists; an installed hash that is in no index is shown as "unknown build" with no update offered.
- Update = the install flow with the new version. Before replacing, the proxy keeps the previous file as `.<name>.wasm.prev` (hidden dotfile, ignored by the scan, which only reads `*.wasm`).
- Rollback: `POST /admin/sniffers/{name}/rollback` swaps `.prev` back. Automatic rollback: if a rescan would drop a module that was loadable a moment ago because the new file fails validation, the scan loads `.prev` instead and logs an error.
- Hot reload is the existing rescan (`reload.rs`); in-flight sniffs finish on the old instance because each call has a fresh `Store`.
- Fleet rollout: per node, through the fan-out; a controller-driven staged rollout is a later design.

## Sniffers repo layout

`plugins/<name>/{manifest.toml, Cargo.toml, src/}`, `index.json` (generated), `README.md`, `CONTRIBUTING.md` (submission rules: license, ABI crate version, tests, no network in tests), CI: build for `wasm32-unknown-unknown`, validate manifest, compute sha256, run the conformance harness from the ABI crate, publish a release with the `.wasm` files and `index.json`. The ABI crate is consumed as a git dependency pinned by tag.

## Risks and open questions

- Egress and secret handling for the UI backend (new outbound dependency). Mitigation: registries are opt-in by config, the default official URL can be disabled by a flag `--no-default-registry`.
- `minisign-verify` crate vetting: check its maintenance status and `cargo audit` before adding (the repo has an audit job).
- SSRF: the UI backend fetches operator-supplied URLs. Every connection, including each redirect hop, goes through a resolver-level guard that refuses loopback, private, link-local, unspecified and unique-local addresses, so a registry cannot redirect the fetch into the internal network. The guard is applied at connect time, not only to the first URL.
- GitHub release asset URLs redirect to a CDN host; the redirect policy must allow `https` redirects across hosts while refusing downgrade to `http`.
- Pinned instances need the manual pin step (settled for the first cut); revisit with controller-managed pins.
