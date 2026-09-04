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
  a `alloc(len) -> ptr` export backed by the module's normal global allocator,
  and an `encode`/`emit_hint` pair that packs a `RouteHint` into the compact
  wire format the host decodes. Every plugin below depends on it.
- `a2s` — recognises Source-engine (CS:GO, TF2, Garry's Mod, Rust, …) A2S
  query packets (`0xFFFFFFFF` + a query-type byte). No hostname to extract;
  tags a recognised query with `key: "a2s"` so a `sniffer` route with an empty
  `host:` list can steer query traffic onto its own pool.
- `minecraft` — parses a Minecraft protocol ≥ 1.7 Handshake packet and
  extracts the `server address` field as the hint's `host` (this is exactly
  how BungeeCord/Velocity-style virtual-host routing works). Strips a Forge
  `\0FML\0…` suffix; lower-cases the host to match the proxy's `sni`-style
  `host:` patterns.
- `regex-firstbytes` — a **template**, not a generic engine: the proxy's
  `sniffer` route / `settings.sniffers` config have no per-plugin parameters
  today, so there's nowhere to hand a module a runtime pattern. This
  demonstrates the bounded, allocation-light matcher shape the roadmap
  describes, hard-coded to recognise an HTTP/1.x request line. Real
  per-instance configurability (e.g. a `modules[].config` blob passed to the
  plugin) is future work — see `HANDOVER.md`.

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
