# Sniffers: modules, manifests and the registry format

A **sniffer** is a WASM module that peeks at the first bytes of a connection or
datagram and returns a routing hint (a hostname or a tag). This page is the format
reference for authors and registry operators. The ABI itself is documented in
[`crates/sniffers/README.md`](../crates/sniffers/README.md) and the module doc of
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
