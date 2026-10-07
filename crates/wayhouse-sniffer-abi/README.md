# wayhouse-sniffer-abi

The guest side of the sniffer ABI: what a sniffer (a WASM module the proxy loads from
`settings.sniffers.dir`) links against. The host side is `crates/wayhouse/src/sniffer_loader.rs`
(see its module documentation for the exact contract and ADR 16a in
`docs/09-technology-choices.md`).

The crate provides:

- an `alloc(len) -> ptr` export backed by the module's normal global allocator,
- `input` / `config` to borrow the two regions the host writes before each call (the peeked bytes,
  and this module's `settings.sniffers.modules[].config` string),
- an `encode` / `emit_hint` pair that packs a `RouteHint` into the compact wire format the host
  decodes,
- the `wayhouse.abi` custom section (currently `0.1`), which the host checks before loading a
  module and refuses on a mismatch or when it is missing. Depending on this crate is all a sniffer
  needs to do to declare its ABI version.

The guest export is `sniff(in_ptr, in_len, cfg_ptr, cfg_len) -> i64`.

The crate is a regular member of this workspace (no dependencies, native tests run in `make check`),
and `wayhouse-ui` reads its `ABI_MAJOR` / `ABI_MINOR`. The official sniffers live in
[`wayhouse-proxy/sniffers`](https://github.com/wayhouse-proxy/sniffers) and pin this crate by git
revision (cargo finds it by name, so keep the crate name); their CI builds them for
`wasm32-unknown-unknown`. Changing the ABI (the wire format or the `wayhouse.abi` version) means:
bump the version here and in `sniffer_loader.rs` together, bump the revision in the sniffers repo,
rebuild and release the sniffers, and update `sniffers.lock` (the e2e job fails until all of that
agrees). See [`docs/sniffers.md`](../../docs/sniffers.md).
