# gsp sniffer plugins

First-party sniffer plugins for the phase 9 WASM loader
(`crates/gsp/src/sniffer_loader.rs`, `docs/08-roadmap.md` Phase 9). This is a
**standalone workspace**, deliberately outside the main one — the same reason
as `crates/gsp-config/fuzz`: these crates build for
`wasm32-unknown-unknown`, are never a dependency of `gsp` / `gsp-core`, and
`make check` on the main workspace must not require the wasm target to be
installed.

## Layout

- `gsp-sniffer-abi` — guest-side helper library implementing the write side of
  the ABI the host documents (`crates/gsp/src/sniffer_loader.rs` module doc):
  an `alloc(len) -> ptr` export backed by the module's normal global allocator,
  `input`/`config` to borrow the two regions the host writes before each call
  (the peeked bytes and this module's `settings.sniffers.modules[].config`
  string), and an `encode`/`emit_hint` pair that packs a `RouteHint` into the
  compact wire format the host decodes. Every plugin below depends on it. The
  guest export is
  `sniff(in_ptr, in_len, cfg_ptr, cfg_len) -> i64` (see ADR 16a).
- `a2s` — recognises Source-engine (CS:GO, TF2, Garry's Mod, Rust, …) A2S
  query packets (`0xFFFFFFFF` + a query-type byte). No hostname to extract;
  tags a recognised query with `key: "a2s"` so a `sniffer` route with an empty
  `host:` list can steer query traffic onto its own pool.
- `minecraft` — parses a Minecraft protocol ≥ 1.7 Handshake packet and
  extracts the `server address` field as the hint's `host` (this is exactly
  how BungeeCord/Velocity-style virtual-host routing works). Strips a Forge
  `\0FML\0…` suffix; lower-cases the host to match the proxy's `sni`-style
  `host:` patterns.
- `quic` — recognises a QUIC Initial packet (long header, version 1, version 2
  or an IETF draft, with the packet type that version uses for Initial and
  connection IDs within the 20-byte limit). The payload is encrypted, so there
  is no hostname; the hint carries `key: "quic"`.
- `wireguard` — recognises a WireGuard handshake initiation (exactly 148 bytes,
  message type 1, three zero reserved bytes) and tags it `key: "wireguard"`.
  Later packets of the flow are not recognised; they ride the session the
  initiation opened.
- `regex-firstbytes` — a bounded, allocation-light, **runtime-configured**
  first-bytes matcher (no `regex` dependency). Its `settings.sniffers.modules[].config`
  string is a tiny pattern language:
  `[key:NAME|] pattern { |pattern }` where `pattern` is
  `[@OFFSET ] (hex:HEX | ascii:TEXT)` — a datagram is recognised when any
  pattern's literal bytes are a prefix of `first[offset..]`, tagging the hint
  with `key` (default `firstbytes`). Examples: `ascii:GET `,
  `key:a2s|@0 hex:ffffffff`. With no `config` it matches nothing. `a2s` and
  `minecraft` take no config and ignore the `cfg_*` region.

Every plugin crate is `crate-type = ["cdylib", "lib"]`: `cargo test` runs its
unit tests as a normal native `rlib` (the `recognise()` function is pure Rust,
host-independent — no pointer/ABI code in the parts that are tested), and
`--target wasm32-unknown-unknown --release` produces the loadable `.wasm`
module.

## Building

```sh
rustup target add wasm32-unknown-unknown   # once
make plugins                                # from the repo root
```

Or directly:

```sh
cd crates/plugins
cargo test --workspace                                             # native unit tests
cargo build --release --target wasm32-unknown-unknown -p a2s -p minecraft -p quic -p regex-firstbytes -p wireguard
```

Output lands in `target/wasm32-unknown-unknown/release/{a2s,minecraft,quic,regex_firstbytes,wireguard}.wasm`
(cargo turns the `regex-firstbytes` crate name's `-` into `_` for the file
name — the loaded sniffer's name is therefore `regex_firstbytes`, not
`regex-firstbytes`, if you copy the file as-is).

## Installing into a running proxy

Copy the built `.wasm` files into the directory named by
`settings.sniffers.dir`, named `<sniffer-name>.wasm` (the host loader names a
sniffer after its file stem):

```sh
mkdir -p /etc/gsp/sniffers
cp crates/plugins/target/wasm32-unknown-unknown/release/a2s.wasm \
   crates/plugins/target/wasm32-unknown-unknown/release/minecraft.wasm \
   /etc/gsp/sniffers/
```

Then reference them by name in a listener's routes:

```yaml
settings:
  sniffers:
    dir: "/etc/gsp/sniffers"

listeners:
  - name: mc
    bind: "0.0.0.0:25565"
    routes:
      - match: { type: sniffer, sniffer: minecraft, host: ["survival.example.net"] }
        action: { pool: survival }
      - match: { type: always }
        action: { pool: lobby }
```

A config reload rescans `dir` live (phase 9 slice 4) — no restart needed to
pick up an added, removed, or rebuilt module. Optional `settings.sniffers.
modules: [{ name, sha256 }]` pins each file's hash for supply-chain
verification; compute it with `sha256sum <file>.wasm`.

### Installing over HTTP instead of `cp`

An instance with `settings.sniffers` configured also accepts modules over its
admin API, so an operator (or the admin GUI's Plugins page) doesn't need
filesystem access to the host:

```sh
curl -X POST --data-binary @a2s.wasm "http://<admin-listen>/admin/sniffers?name=a2s"
curl "http://<admin-listen>/admin/sniffers"                # list, with sha256 + loaded state
curl -X DELETE "http://<admin-listen>/admin/sniffers/a2s"
```

This writes into the same `settings.sniffers.dir` the manual `cp` workflow
above uses, then triggers the same live rescan — the two approaches are
interchangeable, not alternatives with different behavior. `gsp-aggregator`
fans the same upload/delete out to every known instance at once
(`POST`/`DELETE /fleet/sniffers[/{name}]`), and `gsp-ui`'s Plugins page calls
that fan-out. All three routes are `409` on an instance with no
`settings.sniffers` block at all — turning sniffing on from nothing is still
startup-only (see `docs/05-configuration.md`); this endpoint only manages
modules within an already-configured `dir`. If `settings.sniffers.modules`
pins hashes, uploading a brand-new (previously-unpinned) module name loads
the file but the *next* rescan then rejects the whole registry update per
the existing pin-enforcement rule — updating the pin list itself is a config
change made the normal way (file edit / controller revision), not through
this endpoint.

## Size / sandbox notes

Release profile (workspace-wide, `[profile.release]` in this workspace's
`Cargo.toml`) uses `opt-level = "z"`, `lto = true`, `panic = "abort"`,
`strip = true` — these are short-lived, instantiate-per-call modules, so
binary size (currently ~17–21 KiB each) matters more than raw codegen speed.
No plugin here does any I/O, spawns no threads, and imports nothing from the
host beyond the two ABI functions it exports — consistent with the "no WASI,
no host imports" sandbox guarantee in `docs/08` / `docs/07`.
