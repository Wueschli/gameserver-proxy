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
- `regex-firstbytes` — a bounded, allocation-light first-bytes matcher. As of
  the data-plane-completion "per-plugin config" work the ABI carries a
  `settings.sniffers.modules[].config` string (`cfg_*` region); slice **A2**
  wired the plumbing and this crate still ships its compiled-in HTTP/1.x
  request-line pattern, slice **A3** rebuilds it around the config so the
  pattern is genuinely runtime-supplied. `a2s` and `minecraft` take no config
  and ignore the region.

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
cargo build --release --target wasm32-unknown-unknown -p a2s -p minecraft -p regex-firstbytes
```

Output lands in `target/wasm32-unknown-unknown/release/{a2s,minecraft,regex_firstbytes}.wasm`
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

## Size / sandbox notes

Release profile (workspace-wide, `[profile.release]` in this workspace's
`Cargo.toml`) uses `opt-level = "z"`, `lto = true`, `panic = "abort"`,
`strip = true` — these are short-lived, instantiate-per-call modules, so
binary size (currently ~17–21 KiB each) matters more than raw codegen speed.
No plugin here does any I/O, spawns no threads, and imports nothing from the
host beyond the two ABI functions it exports — consistent with the "no WASI,
no host imports" sandbox guarantee in `docs/08` / `docs/07`.
