# Plugin ABI Version Implementation Plan (#183 phase A)

> **Terminology update (2026-10-07).** Leandro split the old "plugin" concept in two. **Sniffers** are the WASM protocol/hostname sniffer modules and live in [`wayhouse-proxy/sniffers`](https://github.com/wayhouse-proxy/sniffers). **Plugins** are integrations with other systems (e.g. the Pelican panel, #213) and live in [`wayhouse-proxy/plugins`](https://github.com/wayhouse-proxy/plugins); their design is still open. Wherever this document says "plugin" or "plugins repo" for a WASM sniffer module, read **sniffer** / **sniffers repo**. Code identifiers, crates and paths were renamed to "sniffer" in the same PR as this banner; older text below may still show the old names.


> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans (project convention: no subagents in implementation threads). Steps use checkbox (`- [ ]`) syntax.

**Goal:** Every sniffer module declares an ABI version in a custom section; the host reads it **without instantiating** and refuses modules from another major or a newer minor.

**Architecture:** The guest ABI crate (`wayhouse-sniffer-abi`) emits a custom section `wayhouse.abi` (4 bytes: major u16 LE, minor u16 LE) through a `#[link_section]` static, so all 8 sniffers get it by depending on the crate. The host parses the module's custom sections with `wasmparser` before `Module::new`, extending `validate` from the #171 plan. Policy: host accepts `major == HOST_MAJOR && plugin_minor <= HOST_MINOR`. Starts at `0.1`; a missing section is a rejection (breaking changes are free before v0.1.0, nothing deployed lacks it).

**Tech Stack:** Rust, `wasmparser` (already in `Cargo.lock` through wasmtime; add as a direct dependency with the same version), WAT for tests.

**Spec:** `docs/superpowers/specs/2026-10-05-plugin-registry-design.md` section "ABI version"; transition plan decision 2.

## Global Constraints

- Section name exactly `wayhouse.abi`; payload exactly 4 bytes: `major: u16 LE`, `minor: u16 LE`; current value `0.1` (`ABI_MAJOR = 0`, `ABI_MINOR = 1`).
- Compatibility: same major, sniffer minor <= host minor. While major is 0, a minor bump may be breaking by SemVer 0.x convention, so for major 0 the rule is **exact minor match**: `plugin == host` (document this; it relaxes at 1.0 by a policy decision, not here).
- Reading the section must not instantiate or run any guest code.
- Prerequisite: `validate` and `ModuleError` from `2026-10-05-sniffer-upload-validation.md` exist (extend them, do not add a second validator).
- Sniffers workspace (`crates/sniffers/`) is a separate Cargo workspace; the host must not depend on it. Share the constants by having the host crate define its own copy and a test that builds a sniffer and compares (Task 3).
- `make check` and `make sniffers` pass.

## Review Focus

- A module with two `wayhouse.abi` sections, a 3-byte payload, or a 6-byte payload must be rejected, not partially read.
- `strip = true` and `lto = true` in the sniffers release profile must not drop the section (the single most likely failure: verify on a real built plugin).
- A module without the section gives an error that tells the author what to add (depend on the ABI crate at version X).
- Unknown extra custom sections (names, producers) are ignored.

---

### Task 1: Host reads and enforces the ABI version

**Files:**
- Modify: `crates/wayhouse/Cargo.toml` (+ `wasmparser`), `crates/wayhouse/src/sniffer_loader.rs`
- Test: same file, `mod tests`

**Interfaces:**
- Produces: `pub const HOST_ABI: AbiVersion = AbiVersion { major: 0, minor: 1 };`
  `#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub struct AbiVersion { pub major: u16, pub minor: u16 }` with `Display` as `major.minor`.
  `pub fn read_abi_version(bytes: &[u8]) -> Result<AbiVersion, ModuleError>`; new variants `ModuleError::AbiMissing`, `ModuleError::AbiMalformed(usize)` (payload length), `ModuleError::AbiIncompatible { sniffer: AbiVersion, host: AbiVersion }`.
  `validate` calls `read_abi_version` after the size checks and before `Module::new`.

- [ ] **Step 1: Write failing tests** with a helper `fn with_abi(wat_body, payload: &[u8]) -> Vec<u8>` that builds the module via `wat::parse_str` and appends the custom section by hand (`wasm-encoder` is not needed: append `0x00, leb(len), leb(name_len), name, payload`): `abi_reads_0_1`, `abi_missing_is_rejected_with_hint`, `abi_payload_of_3_and_6_bytes_rejected`, `abi_two_sections_rejected`, `abi_other_minor_rejected_while_major_is_0` (sniffer `0.2` and `0.0` against host `0.1`), `abi_other_major_rejected`, `abi_extra_custom_sections_ignored`; update the existing `HOST_SNIFFER_WAT` fixtures helper so every valid fixture carries `0.1` (the earlier #171 tests must keep passing).
- [ ] **Step 2: Run** `cargo test -p wayhouse sniffer_loader::tests::abi`. Expected: FAIL.
- [ ] **Step 3: Implement** using `wasmparser::Parser::new(0).parse_all(bytes)` and matching `Payload::CustomSection(c)` where `c.name() == "wayhouse.abi"`; the parse error maps to `ModuleError::Compile`.
- [ ] **Step 4: Run** the same command plus `cargo test -p wayhouse sniffer_loader`. Expected: PASS.
- [ ] **Step 5: Commit** `feat: sniffer modules declare and the host enforces an ABI version (#183)`.

### Task 2: Guest ABI crate emits the section

**Files:**
- Modify: `crates/sniffers/wayhouse-sniffer-abi/src/lib.rs`, its `Cargo.toml` (none expected), `crates/sniffers/README.md`, module doc in `sniffer_loader.rs` (ABI section gets a "Version" paragraph)
- Test: `crates/sniffers/wayhouse-sniffer-abi/src/lib.rs` `mod tests`

**Interfaces:**
- Produces: `pub const ABI_MAJOR: u16 = 0; pub const ABI_MINOR: u16 = 1;` and
  `#[used] #[link_section = "wayhouse.abi"] static ABI_VERSION: [u8; 4] = [0, 0, 1, 0];` (major LE, minor LE) compiled only for `target_arch = "wasm32"` (on native test builds the attribute would put a section in the host binary; use `#[cfg(target_arch = "wasm32")]`).

- [ ] **Step 1: Write failing test** `abi_bytes_match_constants`: asserts `u16::from_le_bytes([b[0],b[1]]) == ABI_MAJOR` and the minor, where `b` is a `pub const ABI_BYTES: [u8; 4]` built from the constants (the static uses `ABI_BYTES`, so the test checks the single source).
- [ ] **Step 2: Run** `cd crates/sniffers && cargo test -p wayhouse-sniffer-abi`. Expected: FAIL.
- [ ] **Step 3: Implement** the constants, `ABI_BYTES`, and the wasm32-only static. Update `README.md` ("a sniffer gets its ABI declaration by depending on this crate; nothing else to do") and the host module doc.
- [ ] **Step 4: Run** the test and `make sniffers`. Expected: PASS; 8 `.wasm` files built.
- [ ] **Step 5: Commit** `feat: the ABI crate stamps every plugin with its ABI version (#183)`.

### Task 3: Conformance against real built sniffers

**Files:**
- Modify: the existing `#[ignore]`d artifact test in `crates/wayhouse` that loads built sniffers (grep `crates/sniffers/target` in `crates/wayhouse/src` and `crates/wayhouse/tests`)

**Interfaces:**
- Consumes: `read_abi_version`, `HOST_ABI`.

- [ ] **Step 1: Write failing test** `built_sniffers_declare_the_host_abi`: for each `*.wasm` in `crates/sniffers/target/wasm32-unknown-unknown/release/`, `assert_eq!(read_abi_version(&bytes).unwrap(), HOST_ABI)`; also asserts at least 8 modules were found (guards against an empty dir passing vacuously).
- [ ] **Step 2: Run** `make sniffers && cargo test -p wayhouse -- --ignored built_sniffers_declare`. Expected: PASS if the section survived `strip`/`lto`. If FAIL with `AbiMissing`, do not guess: first check `wasm-tools objdump` (or `wasm-objdump -x`) on one `.wasm`; the fix candidates in order: keep `strip = "debuginfo"` instead of `true` for the sniffers profile (custom sections other than names are kept by `strip = "debuginfo"`), then fall back to a host-callable exported function `wayhouse_abi() -> i32` read after instantiation (and document the loss of "without instantiating").
- [ ] **Step 3: Confirm CI wiring**: the `sniffers` job already runs `--run-ignored only` for `wayhouse`; no workflow change.
- [ ] **Step 4: Run `make check`.** Commit `test: built sniffers declare the host ABI version (#183)`.

---

## Self-review

Spec coverage: section, no-instantiate read, compatibility rule, all 8 sniffers via the crate, conformance test. Risk recorded with its fallback in Task 3. The registry manifest's `abi` field (manifest plan) uses the same `major.minor` string.
