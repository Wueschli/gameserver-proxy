# Sniffers: modules, manifests and the registry format

A **sniffer** is a WASM module that peeks at the first bytes of a connection or
datagram and returns a routing hint (a hostname or a tag). This page is the format
reference for authors and registry operators. The ABI itself is documented in
[`crates/sniffer-abi/README.md`](../crates/sniffer-abi/README.md) and the module doc of
`crates/wayhouse/src/sniffer_loader.rs`; the design and its reasons are in the
[registry spec](superpowers/specs/2026-10-05-plugin-registry-design.md).

Sniffers are not **plugins**: plugins are integrations with other systems (for
example the Pelican panel) and have their own design, see the
[plugin system design](superpowers/specs/2026-10-07-plugin-system-design.md).

## What the crate does

[`crates/wayhouse-registry`](../crates/wayhouse-registry) holds every rule below as pure
functions (no network, no I/O): parsing and validating an index, picking a compatible
version, verifying a download, and generating an index from manifests. The web UI backend
uses it to browse and install, and the sniffers repository CI uses its
`wayhouse-registry-gen` binary to publish.

## Module requirements

A module must be a core WebAssembly module with no imports, at most 8 MiB, and carry a
`wayhouse.abi` custom section (`major` then `minor`, both `u16` little endian). While the
ABI major is 0 the host requires an exact match of the minor. Depending on
`wayhouse-sniffer-abi` adds the section for you.

## Manifest (`manifest.toml`)

One per sniffer, written by the author. Unknown keys are an error.

| Key                                                 | Meaning                                                                            |
| --------------------------------------------------- | ---------------------------------------------------------------------------------- |
| `name`                                              | Letters, digits, `-` and `_` only (the proxy's module name rule)                   |
| `description`, `license`                            | Shown in the UI                                                                    |
| `version`                                           | SemVer of this sniffer                                                             |
| `abi`                                               | `major.minor` the module was built for; must equal the section in the built module |
| `min_proxy`                                         | Oldest proxy version it runs on                                                    |
| `limits.max_memory_bytes`, `limits.call_timeout_ms` | Resource limits the sniffer is designed for                                        |
| `config` (optional), `homepage` (optional)          | Documentation of the per-module config string; a link                              |

## Index (`index.json`)

The same file format for the official registry and for external ones, served over HTTPS.

```json
{
    "schema": 1,
    "kind": "sniffer",
    "name": "wayhouse official sniffers",
    "sniffers": [
        {
            "name": "minecraft",
            "description": "...",
            "license": "MIT OR Apache-2.0",
            "versions": [
                {
                    "version": "0.1.0",
                    "abi": "0.1",
                    "min_proxy": "0.1.0",
                    "url": "https://.../minecraft.wasm",
                    "sha256": "<64 lowercase hex>",
                    "size": 24576,
                    "signature_url": "https://.../minecraft.wasm.minisig",
                    "limits": { "max_memory_bytes": 1048576, "call_timeout_ms": 50 }
                }
            ]
        }
    ]
}
```

Rules a client enforces: `schema` is 1; `kind` is `sniffer` (`plugin` is reserved for the
plugin registry and rejected for now); names are unique and valid; versions are unique
and sorted newest first; every URL is `https://`; `sha256` is 64 lowercase hex; `size`
is at most 8 MiB; the whole file is at most 1 MiB. Unknown extra fields are ignored so a
newer registry can still be read.

## Choosing a version

For a fleet, the newest version whose `abi` matches the host and whose `min_proxy` is met
by every proxy. When none qualifies the reason is reported (for example "built for sniffer
ABI 0.2, but this proxy speaks ABI 0.1"). If proxy versions are unknown the proxy check is
skipped.

## Verifying a download

In this order: size equals the index, sha256 equals the index, the signature (when one is
published and the official key is known) verifies, and the module itself passes the
requirements above and declares the ABI the index says. A signature that is present but
invalid is an error, never a quiet downgrade to "unsigned". Signatures (minisign) are
optional for now, and an unsigned sniffer is shown as unsigned. The sha256 comes from the
same index as the URL, so for an unsigned sniffer it only protects against corruption.

## Generating an index

```sh
wayhouse-registry-gen generate --name "my registry" --base-url https://example.com/dl \
  --previous index.json --out index.json sniffers/minecraft sniffers/a2s
wayhouse-registry-gen verify index.json
```

Each directory holds `manifest.toml` and `<name>.wasm` (and optionally
`<name>.wasm.minisig`). Artifacts are expected at
`<base-url>/<name>-v<version>/<name>.wasm`. Output is deterministic (sorted, fixed field
order), older versions are kept, and re-running with identical bytes changes nothing. A
published version is immutable: the same version with a different sha256 is refused, so
bump the version instead. A failed run leaves the existing file untouched.

If `--previous` is left out and `--out` already exists, `--out` is read as the previous
index, so a plain regenerate cannot replace a published version. If that file exists but is
not a valid index, the run fails instead of overwriting it; fix or remove the file first.

## Installing from a registry (UI backend)

`wayhouse-ui` fetches indexes and artifacts itself, runs every check above, and only then hands
the verified bytes to the fleet upload, so a proxy needs no internet access and still re-validates
what it receives. The routes are `GET /api/registries` and `GET /api/registries/{id}/sniffers`
(viewer), `POST /api/registries`, `DELETE /api/registries/{id}` and
`POST /api/registries/{id}/install` (operator, the same level as uploading a module by hand).

- **Flags.** `--registries-file <path>` keeps the list across restarts (JSON, written atomically,
  mode 0600; without it changes live in memory only). `--no-default-registry` hides the official
  registry. The official index is `https://raw.githubusercontent.com/wayhouse-proxy/sniffers/main/index.json`.
- **Risk.** Every response says whether a registry is `official` or `external`. External registries
  are installed from at the operator's own risk; the page shows that before every install.
- **Network guards.** Only `https://` is fetched, a redirect is held to the same rules, and a URL or
  hostname that resolves to a private, loopback, link-local or unique-local address is refused.
  Bodies are cut off at the size limits while streaming, and requests time out. The client does not
  use `HTTPS_PROXY`/`HTTP_PROXY`: a forward proxy resolves the destination itself and would bypass
  the address check, so the UI host needs direct outbound HTTPS to the registry.
- **ABI and `min_proxy`.** The ABI is this release's. The aggregator does not report proxy versions
  yet, so `min_proxy` cannot be enforced and the listing says `min_proxy_checked: false`.
- **Signatures.** The official public key is not set yet, so signatures are not checked and installs
  are reported `signed: false`. See the maintainer to-do below.
- **Partial installs.** The reply lists every proxy. A proxy that rejects the module because it pins
  its sniffers (`settings.sniffers.modules`) answers `409` with a reply starting `pinned:`; it is
  reported as `pinned` together with the `name` and `sha256` to add to its pin list. Any other
  refusal (including the `409` of a proxy with `settings.sniffers` unset, which needs a restart) is
  reported with its `detail` text, not as pinned. `200` means every proxy accepted, `207` some did
  or some are pinned, `502` none did and none is pinned.

**Maintainer to-do.** Generate the minisign key pair offline, put the public half in
`crates/wayhouse-ui/src/registry_keys.rs`, and store the secret in the sniffers repository (see the
bootstrap plan). Until then everything installs unsigned.

## Installing by hand

Download `<name>.wasm` from a [sniffers release](https://github.com/wayhouse-proxy/sniffers/releases)
(or build your own) and copy it into the directory named by `settings.sniffers.dir`, named
`<sniffer-name>.wasm`: the proxy names a sniffer after its file stem (so the regex sniffer is
`regex_firstbytes`). Then use the name in a route:

```yaml
settings:
    sniffers:
        dir: "/etc/wayhouse/sniffers"

listeners:
    - name: mc
      bind: "0.0.0.0:25565"
      routes:
          - match: { type: sniffer, sniffer: minecraft, host: ["survival.example.net"] }
            action: { pool: survival }
          - match: { type: always }
            action: { pool: lobby }
```

A config reload rescans `dir` live, so an added, removed or replaced module needs no restart.
`settings.sniffers.modules: [{ name, sha256 }]` optionally pins each file's hash (`sha256sum
<file>.wasm`).

An instance with `settings.sniffers` configured also accepts modules over its admin API, so
no filesystem access is needed:

```sh
curl -X POST --data-binary @a2s.wasm "http://<admin-listen>/admin/sniffers?name=a2s"
curl "http://<admin-listen>/admin/sniffers"                # list, with sha256 + loaded state
curl -X DELETE "http://<admin-listen>/admin/sniffers/a2s"
```

This writes into the same `dir` and triggers the same rescan, so it is interchangeable with
copying files. `wayhouse-aggregator` fans the upload and delete out to every known instance
(`POST`/`DELETE /fleet/sniffers[/{name}]`), and the UI's Sniffers page uses that fan-out. All three
routes answer `409` on an instance with no `settings.sniffers` block (turning sniffing on from
nothing is startup-only, see `docs/05-configuration.md`). If `modules` pins hashes, an unpinned new
module loads but the next rescan rejects the whole update until the pin list is changed the normal
way (file edit or controller revision).

## What this repository tests against the official sniffers

Main builds no sniffer. The loader's own tests use small WAT modules (accept, reject, trap,
timeout, memory cap, wrong ABI version). On top of that, `sniffers.lock` pins the official releases
by tag and sha256, and the `sniffers-e2e` CI job (`make sniffers-fetch`, then the `#[ignore]`d
`wayhouse` tests) loads exactly those bytes through the real loader on amd64 and arm64: the ABI
check, every sniffer's recognition, and the latency gate. It is deliberately **not** a required
check, so an outage of the sniffers repository never blocks a PR; it runs when `wayhouse` or
`sniffers.lock` changes and in the nightly run. Community sniffers are tested by the CI of the
repository that hosts them, not here.

When the ABI changes (`HOST_ABI`, the wire format): update `crates/sniffer-abi`, bump the revision
the sniffers repo pins, release the rebuilt sniffers there (its **Release** workflow), then run
`python3 .github/scripts/update_sniffers_lock.py` here to pin the new tags and hashes from the
published `index.json`, and the e2e job is green again. The same command is how a newer official
sniffer release gets picked up.
