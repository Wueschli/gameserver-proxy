# Sniffers Repo Bootstrap and Moving the Bundled Sniffers Implementation Plan (#183)

> **Terminology update (2026-10-07).** Leandro split the old "plugin" concept in two. **Sniffers** are the WASM protocol/hostname sniffer modules and live in [`wayhouse-proxy/sniffers`](https://github.com/wayhouse-proxy/sniffers). **Plugins** are integrations with other systems (e.g. the Pelican panel, #213) and live in [`wayhouse-proxy/plugins`](https://github.com/wayhouse-proxy/plugins); their design is still open. Wherever this document says "plugin" or "plugins repo" for a WASM sniffer module, read **sniffer** / **sniffers repo**. Code identifiers, crates and paths were renamed to "sniffer" in the same PR as this banner; older text below may still show the old names.


> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans (project convention: no subagents in implementation threads). Steps use checkbox (`- [ ]`) syntax. This plan spans **two repositories**: Part A works in `wayhouse-proxy/sniffers`, Part B in `wayhouse-proxy/wayhouse`. Do Part A completely before Part B.

**Goal:** `wayhouse-proxy/sniffers` becomes the home of all 8 sniffers with CI that builds, tests, signs and publishes them and generates `index.json`; the main repo keeps no sniffers, only the ABI crate and WAT-based loader tests, and fetches pinned sniffer releases for its e2e checks.

**Architecture:** Sniffers repo: one Cargo workspace (`plugins/<name>/` each with `manifest.toml`), the ABI crate as a git dependency pinned by the main repo's `v0.1.0` tag, tag-driven releases per sniffer (`<name>-v<version>`), `wayhouse-registry-gen` (git dependency on main's `wayhouse-registry` crate, same tag) to produce `index.json`. Main: `crates/sniffers/` shrinks to the standalone ABI crate (`crates/sniffer-abi/`), real-plugin tests read `WAYHOUSE_PLUGINS_DIR`, populated by `fetch_sniffers.py` from `sniffers.lock` (release tag + sha256 per sniffer) in a separate, non-required CI job.

**Tech Stack:** Cargo, GitHub Actions, `git subtree split` (history-preserving move), minisign CLI in CI, Python 3 stdlib.

**Spec:** `docs/superpowers/specs/2026-10-05-plugin-registry-design.md` ("Sniffers repo layout"); transition plan section 3 and decision 5 (move all), decision 7 (after v0.1.0). Prerequisites merged: v0.1.0 tagged, `2026-10-05-plugins-repo-push-check.md` succeeded, `2026-10-05-registry-crate-and-index.md` and `2026-10-05-plugin-abi-version.md`.

## Global Constraints

- Starts only after `v0.1.0` exists in `wayhouse-proxy/wayhouse` (the ABI crate is consumed by that tag).
- Sniffer versions are independent SemVer in each `manifest.toml`; first published version of each moved sniffer is `0.1.0`, ABI `0.1`, `min_proxy` `0.1.0`.
- Minisign secret key is a repository secret (`MINISIGN_SECRET_KEY`, `MINISIGN_PASSWORD`) created by the maintainer; threads never see or generate the production key. Until it exists the release workflow publishes unsigned (first-cut rule) and says so in the job summary.
- Published versions are immutable: CI refuses to overwrite a released `.wasm` or change its sha256 in `index.json`.
- Every `uses:` pinned by full commit SHA with a version comment.
- Main's `make check` must not need network, the wasm target or the sniffers repo after Part B.
- Pushes in the sniffers repo go to the thread branch with a PR (the repo's default branch rules are the maintainer's call); repository scope for the thread includes `wayhouse-proxy/sniffers`.

## Review Focus

- `sniffers.lock` hash mismatch must fail the e2e job loudly, and the fetch script must refuse non-`https` and redirects to other schemes.
- An outage of the sniffers repo must not block ordinary PRs in main (the e2e job is not in `ci-ok.needs`; it runs on `sniffers.lock` changes, pushes to main and a nightly schedule).
- After the move, no tracked file in main still says `make sniffers` or `crates/sniffers/`, except under `docs/superpowers/` and in `CHANGELOG.md` (grep gate in the plan's last task).
- History: `git log --follow` on one sniffer's `src/lib.rs` in the new repo reaches the original commits.
- The `index.json` the sniffers repo publishes must parse with `wayhouse_registry::parse_index` (CI runs the real parser) and carries `"kind": "sniffer"`; `wayhouse-registry-gen` emits it, and the golden test covers it.

---

## Part A: the sniffers repository

### Task A1: Import the sniffers with history

**Files:** (in `wayhouse-proxy/sniffers`) `Cargo.toml` (workspace), `plugins/<name>/` for `a2s minecraft openvpn quic raknet regex-firstbytes teamspeak3 wireguard`, `rust-toolchain.toml`, `LICENSE-MIT`, `LICENSE-APACHE`

- [ ] **Step 1: Split history.** In a clone of main at the `v0.1.0` tag: `git subtree split --prefix=crates/sniffers -b plugins-only`. Expected: a branch whose root contains `Cargo.toml`, `wayhouse-sniffer-abi/`, `a2s/` ... Check `git log --oneline plugins-only | wc -l`.
- [ ] **Step 2: Import** into the sniffers repo: `git remote add main-split <path>`, `git fetch main-split plugins-only`, `git merge --allow-unrelated-histories main-split/plugins-only`, then `git mv` each sniffer crate under `plugins/<name>/` and delete the `wayhouse-sniffer-abi` directory (it stays in main). Commit message trailer per session rules.
- [ ] **Step 3: Re-point the ABI dependency** in each sniffer `Cargo.toml` to `wayhouse-sniffer-abi = { git = "https://github.com/wayhouse-proxy/wayhouse", tag = "v0.1.0" }` (cargo finds the crate by name inside the repo; verify with `cargo metadata`; if cargo cannot find it because the crate moved in main, point at the tag that still has it, which is `v0.1.0`: the ABI crate relocates only after this plan's Part B and later tags keep it findable by name).
- [ ] **Step 4: Verify.** `cargo test --workspace` (native unit tests) and `cargo build --release --target wasm32-unknown-unknown --workspace`. Expected: PASS and 8 `.wasm` files. Commit `chore: import the bundled plugins from wayhouse`.

### Task A2: Manifests

**Files:** `plugins/<name>/manifest.toml` x8

- [ ] **Step 1: Write one manifest per sniffer** (fields per spec: `name`, `description` copied from `crates/sniffers/README.md`, `license = "MIT OR Apache-2.0"`, `version = "0.1.0"`, `abi = "0.1"`, `min_proxy = "0.1.0"`, `[limits] max_memory_bytes`, `call_timeout_ms` taken from the example `settings.sniffers` defaults in `docs/05-configuration.md`, `config` documentation string where the sniffer reads one, e.g. `regex-firstbytes` and `a2s` if they do; read each sniffer's source for its `config` use).
- [ ] **Step 2: Add a CI-less check script** `scripts/check_manifests.sh` running `wayhouse-registry-gen --check plugins/*/manifest.toml` (add a `--check` mode to the generator in the main repo only if missing: it parses manifests and exits non-zero on errors; if the main repo lacks it, do that as a one-task PR there first). Expected: exit 0.
- [ ] **Step 3: Commit** `feat: manifests for the eight plugins`.

### Task A3: Build and test CI

**Files:** `.github/workflows/ci.yml`

- [ ] **Step 1: Write the workflow**: on PR and push to main: native tests (`cargo nextest run` or `cargo test`), wasm build, manifest check, `wayhouse-registry-gen` dry run into a temp `index.json` then `wayhouse-registry-gen --verify index.json` (parses with the real parser), size table in the job summary. One aggregate job `ci-ok` like main's.
- [ ] **Step 2: Run on a PR** (when the Actions outage is over). Expected: green. **Commit** `ci: build, test and validate the plugins`.

### Task A4: Release workflow and index

**Files:** `.github/workflows/release.yml`, `scripts/publish_index.sh`

- [ ] **Step 1: Write the workflow**: trigger `push` tags `*-v[0-9]*`; parse `<name>` and `<version>`; require tag `==` the manifest `name-v<version>`; build that sniffer for wasm; sign with `minisign -S` when `MINISIGN_SECRET_KEY` is set (write the key to a temp file with mode 0600, remove it in an `always()` step); create a GitHub release `<name>-v<version>` with `<name>.wasm` and `<name>.wasm.minisig`; run `wayhouse-registry-gen --previous index.json` (it refuses changed hashes of published versions); commit the new `index.json` to `main` as `github-actions[bot]` (the maintainer must allow that in the repo's rules; if it cannot push, upload `index.json` as a release asset on a rolling release `index` instead and change `DEFAULT_REGISTRY_URL` in the UI crate to match).
- [ ] **Step 2: Dry run** with a throwaway tag in a fork or with `workflow_dispatch` and `--dry-run` input. Expected: a generated `index.json` artifact that passes the parser.
- [ ] **Step 3: Commit** `ci: tag-driven plugin releases with signing and index generation`.

### Task A5: README and CONTRIBUTING

**Files:** `README.md`, `CONTRIBUTING.md`

- [ ] **Step 1: Write** README (what this is, official vs external, how to install via the UI, trust model in three lines linking to `docs/sniffers.md` in main) and CONTRIBUTING (submission rules: licence, ABI crate tag, tests required, no network or clock in tests, manifest fields, review policy, how a release is cut, the "at your own risk" line for external registries).
- [ ] **Step 2: Cut the first releases**: tags `<name>-v0.1.0` for all 8 (maintainer approves the tag push or a thread with rights does it); verify `index.json` lists 8 sniffers and that the UI client from the install plan can browse and install one (manual check, record in the PR).
- [ ] **Step 3: Commit** `docs: README and CONTRIBUTING`.

---

## Part B: the main repository

### Task B1: `sniffers.lock`, fetch script, and loader tests without bundled sniffers

**Files:**
- Create: `sniffers.lock`, `.github/scripts/fetch_sniffers.py`, `.github/scripts/fetch_sniffers_test.py`
- Modify: `crates/wayhouse/src/sniffer_loader.rs` (the three `#[ignore]` tests around lines 684, 787, 840; plus new WAT-based tests)

**Interfaces:**
- Produces: `sniffers.lock` format (TOML): `repo = "wayhouse-proxy/sniffers"`, then `[plugins.<name>] tag = "<name>-v0.1.0"`, `sha256 = "<hex>"`, `file = "<name>.wasm"`. `fetch_sniffers.py [--lock sniffers.lock] [--out target/plugins]` downloads `https://github.com/<repo>/releases/download/<tag>/<file>`, verifies sha256, refuses non-https and a hash mismatch, exits non-zero listing every failure; prints the out dir. Env `WAYHOUSE_PLUGINS_DIR` is read by the ignored tests (default `target/plugins`).

- [ ] **Step 1: Write failing tests** `fetch_sniffers_test.py` with a local `http.server` over a temp dir and an `--allow-insecure-test-url` hidden flag only usable when env `WAYHOUSE_FETCH_TEST=1`: `downloads_and_verifies`, `hash_mismatch_fails_and_names_the_plugin`, `refuses_http_without_the_test_flag`, `partial_failure_reports_all`, `second_run_skips_files_with_matching_hash`.
- [ ] **Step 2: Run** `python3 .github/scripts/fetch_sniffers_test.py`. Expected: FAIL. **Step 3: Implement.** **Step 4: Run.** Expected: PASS.
- [ ] **Step 5: Add WAT conformance tests** to `sniffer_loader.rs` for the cases the real sniffers used to cover that are not yet covered by WAT fixtures: `wat_module_trap_is_a_call_error`, `wat_module_infinite_loop_hits_the_timeout`, `wat_module_memory_grow_past_cap_fails`, `wat_module_wrong_abi_version_rejected` (some exist from earlier plans; add only the missing ones, list which in the PR).
- [ ] **Step 6: Change the three ignored tests** to read `WAYHOUSE_PLUGINS_DIR` instead of `crates/sniffers/target/...` and skip with a clear `#[ignore = "needs WAYHOUSE_PLUGINS_DIR; run 'make sniffers-fetch' first"]`.
- [ ] **Step 7: Run** `make check`. Create `sniffers.lock` with the 8 real hashes (copy from the sniffers repo release assets). **Commit** `feat: fetch pinned plugins for the e2e tests (#183)`.

### Task B2: Remove the sniffers from main, keep the ABI crate

**Files:**
- Move: `crates/sniffers/wayhouse-sniffer-abi/` to `crates/sniffer-abi/` with its own `[workspace]` `Cargo.toml` and `Cargo.lock`
- Delete: `crates/sniffers/*` except what moved (8 sniffer crates, `crates/sniffers/Cargo.toml`, `Cargo.lock`, `README.md`)
- Modify: `Makefile` (`sniffers` target replaced by `sniffers-fetch` and `sniffer-abi-test`), `.github/workflows/ci.yml` (remove `sniffers` and `sniffers-arm64` jobs; add `sniffer-abi` job running `cd crates/sniffer-abi && cargo test`; add `sniffers-e2e` job, **not** in `ci-ok.needs`, triggers: changes to `sniffers.lock` or `crates/wayhouse/**`, push to main, nightly `schedule`; steps: install nextest, `python3 .github/scripts/fetch_sniffers.py`, `cargo nextest run -p wayhouse --release --run-ignored only`), `.github/scripts/changes.py` (+ test: area `sniffers` now maps to `sniffers.lock`, `crates/sniffer-abi/`), `.github/actions/cargo-cache/action.yml` and `.github/scripts/cargo_audit.sh` (drop sniffers paths, add `crates/sniffer-abi`), `README.md`, `AGENTS.md`, `HANDOVER.md`, docs that mention `crates/sniffers` (grep list: `docs/05-configuration.md`, `docs/07-security-ddos.md`, `docs/08-roadmap.md`, `docs/README.md`, `crates/wayhouse-ui/web/README.md`), `crates/wayhouse/src/sniffer_loader.rs` module doc path

**Interfaces:**
- Consumes: Part A releases, `sniffers.lock`.
- Produces: `crates/sniffer-abi` (crate name `wayhouse-sniffer-abi` unchanged so the sniffers repo's git dependency by name keeps resolving at later tags).

- [ ] **Step 1: Gate test first.** Add `.github/scripts/no_bundled_sniffers_test.py` asserting `crates/sniffers` does not exist and no tracked file matches `crates/sniffers/` or `make sniffers` except in `docs/superpowers/` and `CHANGELOG.md`; run it. Expected: FAIL.
- [ ] **Step 2: Do the move and edits** listed above with `git mv`.
- [ ] **Step 3: Run** `python3 .github/scripts/no_bundled_sniffers_test.py`, `python3 .github/scripts/changes_test.py`, `make check`, `cd crates/sniffer-abi && cargo test`, then `make sniffers-fetch && cargo test -p wayhouse --release -- --ignored`. Expected: all PASS.
- [ ] **Step 4: Update `HANDOVER.md`**, `CHANGELOG.md` is automated. **Commit** `refactor!: move the bundled plugins to wayhouse-proxy/sniffers (#183)` (the `!` marks the breaking layout change for release-please).

---

## Self-review

Spec coverage: bootstrap after v0.1.0 (decision 7), move all with history (decision 5), main keeps ABI crate and WAT tests with a pinned-release e2e fixture (section 3 of the transition plan), index generation and signing (decision 4, optional). One correction: the transition plan says the ABI crate is a "git dependency pinned by tag"; the tag must be a main tag that still contains the crate, hence `v0.1.0` and the same crate name after relocation. Maintainer-dependent: `MINISIGN_*` secrets, whether the bot may push `index.json` to `main` in the sniffers repo, tag push rights.
