# Registry Crate: Index Format, Verification, Compatibility Implementation Plan (#183 phase B)

> **Terminology update (2026-10-07).** Leandro split the old "plugin" concept in two. **Sniffers** are the WASM protocol/hostname sniffer modules and live in [`wayhouse-proxy/sniffers`](https://github.com/wayhouse-proxy/sniffers). **Plugins** are integrations with other systems (e.g. the Pelican panel, #213) and live in [`wayhouse-proxy/plugins`](https://github.com/wayhouse-proxy/plugins); their design is still open. Wherever this document says "plugin" or "plugins repo" for a WASM sniffer module, read **sniffer** / **sniffers repo**. Code identifiers, crate names, config keys, file names and the "plugin ABI" keep their old names for now (rename pending a decision).


> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans (project convention: no subagents in implementation threads). Steps use checkbox (`- [ ]`) syntax.

**Goal:** A small, network-free library crate that defines the sniffer registry index and manifest formats and implements every check on them: parsing and validation, version and ABI compatibility, sha256 and minisign verification, and index generation from manifests.

**Architecture:** New workspace crate `crates/wayhouse-registry` (pure functions over bytes and strings, no I/O, no async, no network) so it is trivially testable and reusable by the UI backend (client side, next plan) and the sniffers repo CI (generator, via a git dependency pinned by tag). The network client lives in `wayhouse-ui`.

**Tech Stack:** Rust, serde/serde_json, `toml`, `semver`, `sha2` (workspace), `minisign-verify`, `wasmparser` (to read the ABI section without the host).

**Spec:** `docs/superpowers/specs/2026-10-05-plugin-registry-design.md` (sections "Registry format" and "Trust"). Prerequisites merged: v0.1.0 tagged, `2026-10-05-plugin-abi-version.md`.

## Global Constraints

- Schema constants: `INDEX_SCHEMA = 1`; sniffer `name` matches `^[A-Za-z0-9_-]+$` (same as `valid_module_name` in `admin.rs`); `url` must be `https://`; `sha256` is 64 lowercase hex; `size <= 8 * 1024 * 1024` (`MAX_MODULE_BYTES`, equal to the proxy's; a test asserts equality by reading the constant value from the source of truth: duplicate the number and keep a comment pointing at `sniffer_loader.rs`); index file size cap 1 MiB.
- ABI string is `major.minor`; compatibility reuses the host rule (exact match while major is 0).
- No panics on hostile input: every parser returns `Result`; fuzz-style tests with garbage JSON.
- Versions stay 0.x for this crate (`version.workspace = true`). New dependency crates need `cargo audit` clean (`make audit`) and a licence check of `minisign-verify` (MIT/Apache expected; verify before adding).
- `make check` including `test-minimal` (the new crate is not part of the minimal build, so no change there).

## Review Focus

- An index with duplicate sniffer names, duplicate versions, an unsorted `versions` array, `http://` urls, or an uppercase sha must each give a distinct, readable error.
- Compatibility must pick the newest version that satisfies both `abi` and `min_proxy` and say why when none does (the UI shows the reason).
- A tampered artifact (one flipped byte) must fail sha256 first, then signature, with separate errors.
- The generator must be deterministic (same inputs, byte-identical `index.json`) so CI diffs are meaningful.

---

### Task 1: Crate skeleton and index types

**Files:**
- Create: `crates/wayhouse-registry/Cargo.toml`, `src/lib.rs`, `src/index.rs`
- Modify: root `Cargo.toml` (member + `[workspace.dependencies]` entries for `semver`, `toml`, `minisign-verify`, `wasmparser`)

**Interfaces:**
- Produces:
  `pub struct Index { pub schema: u32, pub name: String, pub plugins: Vec<PluginEntry> }`,
  `pub struct PluginEntry { pub name: String, pub description: String, pub license: String, pub homepage: Option<String>, pub versions: Vec<VersionEntry> }`,
  `pub struct VersionEntry { pub version: semver::Version, pub abi: String, pub min_proxy: semver::Version, pub url: String, pub sha256: String, pub size: u64, pub signature_url: Option<String>, pub limits: Limits, pub config: Option<String> }`,
  `pub struct Limits { pub max_memory_bytes: u64, pub call_timeout_ms: u64 }`,
  `pub fn parse_index(bytes: &[u8]) -> Result<Index, IndexError>` (size cap, JSON parse, `validate`), `impl Index { pub fn validate(&self) -> Result<(), IndexError> }`, `pub enum IndexError` (one variant per rule in Review Focus, `Display` texts name the sniffer and field).

- [ ] **Step 1: Write failing tests** in `index.rs`: `parses_the_example_from_the_spec` (paste the JSON from the spec as a fixture), `rejects_schema_2`, `rejects_duplicate_plugin_name`, `rejects_duplicate_version`, `rejects_unsorted_versions`, `rejects_http_url`, `rejects_uppercase_or_short_sha`, `rejects_bad_name` (`../evil`, empty, space), `rejects_oversize_module`, `rejects_oversize_index_bytes`, `rejects_garbage_json_without_panic`.
- [ ] **Step 2: Run** `cargo test -p wayhouse-registry index`. Expected: FAIL (crate missing).
- [ ] **Step 3: Implement** types with `serde(deny_unknown_fields)` **off** for forward compatibility (a newer registry may add fields), plus the validation.
- [ ] **Step 4: Run** the same command. Expected: PASS. **Commit** `feat: wayhouse-registry crate with the index format (#183)`.

### Task 2: Compatibility selection

**Files:**
- Create: `crates/wayhouse-registry/src/compat.rs`

**Interfaces:**
- Consumes: `Index`, `VersionEntry`.
- Produces: `pub struct Environment { pub abi: String, pub proxy_versions: Vec<semver::Version> }` (all proxies the sniffer will be installed on: every one must satisfy `min_proxy`), `pub fn select(entry: &PluginEntry, env: &Environment) -> Result<&VersionEntry, Incompatible>`, `pub enum Incompatible { Abi { newest: String, host: String }, ProxyTooOld { needs: semver::Version, oldest_proxy: semver::Version }, NoVersions }`, `pub fn abi_matches(plugin: &str, host: &str) -> Result<bool, AbiParseError>` (exact match while major is 0, `plugin_minor <= host_minor` for major >= 1).

- [ ] **Step 1: Write failing tests**: `selects_newest_compatible`, `skips_newer_version_with_other_abi`, `reports_abi_when_nothing_matches`, `reports_oldest_proxy_when_min_proxy_too_high`, `abi_zero_major_requires_exact_minor`, `abi_major_1_allows_older_minor`, `abi_rejects_garbage_string`, `prerelease_proxy_counts_as_its_release` (proxy `0.1.0-rc.1` satisfies `min_proxy 0.1.0`: use the stripped pre-release for the comparison and document it).
- [ ] **Step 2: Run** `cargo test -p wayhouse-registry compat`. Expected: FAIL.
- [ ] **Step 3: Implement** with `semver`; the `Incompatible` display texts are the UI's reasons.
- [ ] **Step 4: Run.** Expected: PASS. **Commit** `feat: registry version compatibility selection (#183)`.

### Task 3: Artifact verification

**Files:**
- Create: `crates/wayhouse-registry/src/verify.rs`

**Interfaces:**
- Consumes: `VersionEntry`.
- Produces: `pub fn verify_artifact(v: &VersionEntry, bytes: &[u8], signature: Option<&[u8]>, official_key: Option<&minisign_verify::PublicKey>) -> Result<Verified, VerifyError>`, `pub struct Verified { pub signed: bool }`, `pub enum VerifyError { Size{expected:u64,got:usize}, Sha256{expected:String,got:String}, Signature(String), SignatureRequiredButMissing, Module(String) }`, `pub fn check_module(bytes: &[u8]) -> Result<AbiDecl, VerifyError>` (reads `wayhouse.abi` with `wasmparser`, rejects any import) and `pub struct AbiDecl { pub major: u16, pub minor: u16 }`.
- Rule: signature present and key given -> must verify (failure is an error, never a downgrade to unsigned); signature absent -> `signed: false` (optional in the first cut).

- [ ] **Step 1: Write failing tests** with a generated key pair (`minisign` crate as a **dev-dependency** to sign a fixture, or a checked-in fixture signature produced once with the `minisign` CLI and a throwaway key; prefer the checked-in fixture and document how it was made in a comment): `ok_unsigned`, `ok_signed`, `bad_sha_is_sha_error_before_signature`, `flipped_byte_with_valid_sha_still_fails_signature` (fixture where the index sha was recomputed for the tampered bytes), `signature_present_but_no_key_is_unsigned_ok`, `size_mismatch`, `module_with_import_rejected`, `module_without_abi_section_rejected`, `module_abi_is_read`.
- [ ] **Step 2: Run** `cargo test -p wayhouse-registry verify`. Expected: FAIL.
- [ ] **Step 3: Implement** in the order size, sha256, signature, module.
- [ ] **Step 4: Run.** Expected: PASS. **Commit** `feat: registry artifact verification (#183)`.

### Task 4: Manifest and deterministic index generator

**Files:**
- Create: `crates/wayhouse-registry/src/manifest.rs`, `src/generate.rs`, `src/bin/wayhouse-registry-gen.rs`
- Test: same files plus `tests/generate_golden.rs` with a golden `index.json`

**Interfaces:**
- Produces: `pub struct Manifest { pub name: String, pub description: String, pub license: String, pub version: semver::Version, pub abi: String, pub min_proxy: semver::Version, pub limits: Limits, pub config: Option<String>, pub homepage: Option<String> }` parsed from `manifest.toml` via `pub fn parse_manifest(text: &str) -> Result<Manifest, ManifestError>`; `pub struct Artifact { pub manifest: Manifest, pub bytes: Vec<u8>, pub url: String, pub signature_url: Option<String> }`; `pub fn generate(registry_name: &str, artifacts: &[Artifact], previous: Option<&Index>) -> Result<Index, GenerateError>` (computes sha256 and size, checks that the built module's ABI declaration equals `manifest.abi`, merges with `previous` so older versions are kept, sorts sniffers by name and versions newest first, validates); CLI `wayhouse-registry-gen --name <n> --previous index.json --out index.json <dir with manifest.toml + .wasm + base-url>` printing the changed entries.

- [ ] **Step 1: Write failing tests**: `manifest_round_trip`, `manifest_rejects_unknown_abi_format`, `generate_computes_sha_and_size`, `generate_rejects_abi_mismatch_between_manifest_and_module`, `generate_keeps_previous_versions`, `generate_is_deterministic` (two runs, byte-identical output), `generate_rejects_a_version_already_published_with_a_different_sha` (immutability of a published version), golden test comparing to `tests/golden/index.json`.
- [ ] **Step 2: Run** `cargo test -p wayhouse-registry generate manifest`. Expected: FAIL.
- [ ] **Step 3: Implement**; the CLI is a thin `clap` wrapper (clap is a workspace dependency).
- [ ] **Step 4: Run** the tests, `make check`, `make audit`. **Commit** `feat: registry manifest and deterministic index generator (#183)`.

### Task 5: Docs

**Files:**
- Create: `docs/plugins.md` (format reference: index, manifest, ABI, limits, trust model, how to write and submit a sniffer; link to `crates/plugins/README.md` until the move); modify `docs/README.md` (list the chapter), `AGENTS.md` (layout and "when you touch X" row)

- [ ] **Step 1: Write** `docs/plugins.md` from the spec's Registry format and Trust sections; no new decisions.
- [ ] **Step 2: Run** the docs checks (`make docs-fmt-check`, link checker) if the docs overhaul landed. **Commit** `docs: plugin registry format reference (#183)`.

---

## Self-review

Spec coverage: index and manifest formats, ABI and `min_proxy` compatibility, sha256 then minisign verification (optional signature), deterministic generator for the plugins-repo CI. Network, UI and persistence of external registries are the next plan. Types used by later plans: `Index`, `PluginEntry`, `VersionEntry`, `Environment`, `select`, `verify_artifact`, `check_module`, `generate`.
