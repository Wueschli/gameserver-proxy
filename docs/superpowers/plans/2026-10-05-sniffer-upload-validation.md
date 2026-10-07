# Sniffer Module Validation Implementation Plan (#171)

> **Terminology update (2026-10-07).** Leandro split the old "plugin" concept in two. **Sniffers** are the WASM protocol/hostname sniffer modules and live in [`wayhouse-proxy/sniffers`](https://github.com/wayhouse-proxy/sniffers). **Plugins** are integrations with other systems (e.g. the Pelican panel, #213) and live in [`wayhouse-proxy/plugins`](https://github.com/wayhouse-proxy/plugins); their design is still open. Wherever this document says "plugin" or "plugins repo" for a WASM sniffer module, read **sniffer** / **sniffers repo**. Code identifiers, crates and paths were renamed to "sniffer" in the same PR as this banner; older text below may still show the old names.


> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans (project convention: no subagents in implementation threads). Steps use checkbox (`- [ ]`) syntax.

**Goal:** `POST /admin/sniffers` rejects anything that is not a loadable sniffer module, and a bad `.wasm` file on disk can never block a rescan or startup.

**Architecture:** One validation function in `sniffer_loader.rs` (compile, check imports and exports, instantiate under the memory cap) is used by both the upload handler (reject before writing, atomic write) and `SnifferLoader::scan` (skip a bad file instead of failing the whole scan). The admin module receives validation as a closure so it stays independent of the wasmtime build feature.

**Tech Stack:** Rust, wasmtime 48, axum 0.8, `wat`.

**Spec:** issue #171; `crates/wayhouse/src/sniffer_loader.rs` module doc (ABI); reuse of this validator by `2026-10-05-plugin-install-and-trust.md`.

## Global Constraints

- ABI today: exports `memory`, `alloc(i32)->i32`, `sniff(i32,i32,i32,i32)->i64`; core module, **no imports** (no WASI).
- Pin enforcement stays as is: with `settings.sniffers.modules` set, an unpinned or hash-mismatched file fails the whole scan (security property, keep).
- `make check` (fmt, clippy `-D warnings`, tests, `test-minimal`) must pass; the minimal build (`--no-default-features`) uses `sniffer_loader_disabled.rs`, keep both in step.
- Maximum module size: `MAX_MODULE_BYTES = 8 * 1024 * 1024` (8 MiB; the largest bundled sniffer is far smaller; check `ls -la crates/plugins/target/wasm32-unknown-unknown/release/*.wasm` and note the number in the PR).

## Review Focus

- A zero-byte upload and a valid-header-but-truncated module must give 400, not 500, and leave no file behind.
- A module with an import (for example WASI `fd_write`) must be rejected with a message naming the import.
- Two concurrent uploads of the same name must not interleave bytes (atomic rename).
- The default axum body limit (2 MiB) must not silently reject a valid 3 MiB sniffer with a confusing error.
- A garbage file already in `dir` must not stop the other modules from loading at startup.
- On an instance with pins, an upload that the pins would reject must never reach the disk: a stray unpinned file makes the next startup fatal (`build_sniffers(...)?` in `main.rs`) and blocks every other module's rescan.

---

### Task 1: `validate_module` in the loader

**Files:**
- Modify: `crates/wayhouse/src/sniffer_loader.rs`, `crates/wayhouse/src/sniffer_loader_disabled.rs`
- Test: same file, `mod tests` (reuse `HOST_SNIFFER_WAT`)

**Interfaces:**
- Produces: `pub const MAX_MODULE_BYTES: usize = 8 * 1024 * 1024;`
  `pub enum ModuleError { TooLarge{len:usize,max:usize}, Empty, Compile(String), UnexpectedImport(String), MissingExport(&'static str), WrongSignature(&'static str), Instantiate(String) }` implementing `Display` + `std::error::Error`.
  `impl SnifferLoader { pub fn validate(&self, bytes: &[u8], max_memory_bytes: usize) -> Result<(), ModuleError> }`.
  Disabled build: same enum is not needed; `sniffer_loader_disabled.rs` exposes nothing new (admin gets `None`).

- [ ] **Step 1: Write failing tests** `validate_accepts_the_host_fixture`, `validate_rejects_empty`, `validate_rejects_garbage_bytes` (assert `Compile`), `validate_rejects_oversize` (`MAX_MODULE_BYTES + 1` zero bytes gives `TooLarge`), `validate_rejects_import` (WAT `(import "wasi_snapshot_preview1" "fd_write" (func))`; assert `UnexpectedImport` text contains `fd_write`), `validate_rejects_missing_sniff`, `validate_rejects_wrong_sniff_signature` (`sniff` taking one param), `validate_rejects_start_function_that_traps` (`(start $f)` with `unreachable`; assert `Instantiate`).
- [ ] **Step 2: Run** `cargo test -p wayhouse sniffer_loader::tests::validate -- --nocapture`. Expected: FAIL (not defined).
- [ ] **Step 3: Implement** `validate`: length checks first, `Module::new`, iterate `module.imports()` (any import is an error), look up `module.get_export` for the three names and compare `FuncType` with `FuncType::new(engine, [I32;4], [I64])` and the alloc and memory kinds, then `Linker::new`, `Store` with `StoreLimitsBuilder::memory_size(max_memory_bytes)` and the same epoch deadline settings `WasmSniffer::call` uses (extract a private `fn new_store(&self, max_memory_bytes) -> Store<StoreState>` shared by both so limits cannot drift), `instantiate` only (do not call `sniff`).
- [ ] **Step 4: Run** the same command. Expected: PASS.
- [ ] **Step 5: Commit** `feat: validate sniffer modules before they are accepted (#171)`.

### Task 2: scan skips unloadable files

**Files:**
- Modify: `crates/wayhouse/src/sniffer_loader.rs` (`scan`)

**Interfaces:**
- Consumes: `validate` from Task 1 (scan calls it with `cfg.max_memory_bytes`).
- Produces: `scan` returns `Ok` with the loadable subset; a skipped file logs `tracing::error!(sniffer=%name, error=%e, "sniffer module rejected, skipping")`.

- [ ] **Step 1: Write failing tests** `scan_skips_garbage_file_and_loads_the_rest` (dir with `good.wasm` = compiled fixture, `junk.wasm` = `b"not wasm"`; assert map keys == `["good"]`), `scan_still_fails_on_pin_mismatch` (pinned config, wrong sha; assert `Err`, regression guard for the security property), `scan_skips_module_with_import`.
- [ ] **Step 2: Run** `cargo test -p wayhouse sniffer_loader::tests::scan`. Expected: FAIL on the first test (currently the whole scan errors).
- [ ] **Step 3: Implement**: replace the `Module::new(...)?` failure path by validate-and-skip; pin checks stay before it and stay fatal.
- [ ] **Step 4: Run** the same command. Expected: PASS. Also run `cargo test -p wayhouse reload` (rescan behaviour).
- [ ] **Step 5: Commit** `fix: one bad sniffer file no longer fails the whole rescan (#171)`.

### Task 3: validate, then write atomically, in the upload handler

**Files:**
- Modify: `crates/wayhouse/src/admin.rs` (`AdminState`, `upload_sniffer`, router), `crates/wayhouse/src/main.rs` (construct the closure)
- Test: `crates/wayhouse/src/admin.rs` `mod tests` (extends `upload_list_and_delete_a_sniffer_module_round_trips`)

**Interfaces:**
- Consumes: `SnifferLoader::validate`, `MAX_MODULE_BYTES`.
- Produces: `AdminState.sniffer_pins: std::sync::Arc<dyn Fn() -> Vec<wayhouse_config::SnifferModulePin> + Send + Sync>` (reads the live config snapshot, so a reload that changes pins is seen; find how `sniffers_dir` is kept current and follow that), and `AdminState.sniffer_validator: Option<std::sync::Arc<dyn Fn(&[u8]) -> Result<(), String> + Send + Sync>>`; HTTP behaviour: `400` plain-text `invalid sniffer module: <ModuleError>` for any validation error, `413` above `MAX_MODULE_BYTES`, `409` `pinned: ...` (body names the expected pin, or says the name is not pinned, with the sha256 of the upload) when `sniffer_pins()` is non-empty and the upload's name is unpinned or its sha256 differs, **writing nothing**; `200` as before on success.

- [ ] **Step 1: Write failing tests** `upload_rejects_garbage_with_400_and_writes_nothing` (assert the dir is empty afterwards), `upload_rejects_empty_body`, `upload_rejects_oversize_with_413`, `upload_accepts_valid_module_larger_than_2mib` (pad a valid module with a custom section to 3 MiB; this pins the body limit), `upload_writes_via_rename_and_leaves_no_tmp_file`, `upload_to_pinned_instance_with_other_hash_is_409_and_writes_nothing`, `upload_unpinned_name_on_pinned_instance_is_409_and_writes_nothing`, `upload_matching_pin_is_accepted`, `pin_check_uses_live_pins_after_reload`.
- [ ] **Step 2: Run** `cargo test -p wayhouse admin::tests::upload`. Expected: FAIL.
- [ ] **Step 3: Implement**: add `DefaultBodyLimit::max(MAX_MODULE_BYTES)` on the upload route only; validate via `sniffer_validator` (if `None`, behave as today only when the sniffers dir exists, which cannot happen without the loader; return 409 `sniffers_disabled()`); write to `<dir>/.<name>.wasm.tmp` then `std::fs::rename` to `<name>.wasm` (scan only reads `*.wasm`, so the dotfile is ignored); on rename error remove the tmp file. `main.rs` builds the closure from the loader (`Arc<SnifferLoader>` clone) and `cfg.sniffers.max_memory_bytes`; the `test-minimal` build passes `None`.
- [ ] **Step 4: Run** the same command plus `make check`. Expected: PASS.
- [ ] **Step 5: Update docs**: `docs/` chapter that describes `/admin/sniffers` (grep `admin/sniffers docs`), mention the 400/413 behaviour and the size cap. Commit `feat: reject invalid sniffer uploads and write them atomically (#171)`.

---

## Self-review

Spec coverage: reject-before-write (T3), never block rescan or startup (T2), validator reusable by the registry install path (T1 signature takes bytes; the install plan's UI backend uses the same checks through the shared `wayhouse-registry` crate and the proxies re-run this validator on every upload). Issue #183/#184 text still cites "#171 invalid .wasm"; fix the wording when closing (comment on both issues).
