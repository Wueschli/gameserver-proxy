# Versioning, Changelog and Release Automation Implementation Plan (#188)

> **Terminology update (2026-10-07).** Leandro split the old "plugin" concept in two. **Sniffers** are the WASM protocol/hostname sniffer modules and live in [`wayhouse-proxy/sniffers`](https://github.com/wayhouse-proxy/sniffers). **Plugins** are integrations with other systems (e.g. the Pelican panel, #213) and live in [`wayhouse-proxy/plugins`](https://github.com/wayhouse-proxy/plugins); their design is still open. Wherever this document says "plugin" or "plugins repo" for a WASM sniffer module, read **sniffer** / **sniffers repo**. Code identifiers, crates and paths were renamed to "sniffer" in the same PR as this banner; older text below may still show the old names.


> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans (project convention: no subagents in implementation threads). Steps use checkbox (`- [ ]`) syntax.

**Goal:** One written version policy for 0.x, CI that keeps `Cargo.toml` and `Cargo.lock` in step and can never reach 1.0.0, a changelog, and release-please opening the release PRs.

**Architecture:** The workspace version in the root `Cargo.toml` is the single source of truth (already true: `check_release.py`, `--version`, `wayhouse_build_info`). A stdlib Python policy script guards it in CI and in `release.yml`. release-please (manifest mode, `simple` release type with `extra-files` for the two TOML files) maintains `CHANGELOG.md` and the release PR; its PRs get CI through a `workflow_dispatch` of `ci.yml`, because PRs created with `GITHUB_TOKEN` do not trigger workflows and `ci-ok` is required.

**Tech Stack:** Python 3 stdlib (tomllib), GitHub Actions, release-please-action, conventional commits.

**Spec:** issue #188; decisions 1 and 8 in `/mnt/project-files/wayhouse/transition-plan.md`; branching section from `2026-10-05-release-branching-doc.md` (`RELEASING.md` exists first).

## Global Constraints

- Versions stay 0.x. Nothing in this repo may produce `1.0.0`; going to 1.0 is the maintainer's explicit call and needs a deliberate policy-script edit.
- 0.x rules: minor bump = anything breaking (config schema, CLI flags, admin/fleet HTTP APIs, wire protocols, plugin ABI, metric names, on-disk formats); patch = fixes and compatible additions.
- `check_release.py` requires tag `== v<workspace version>`; so `v0.1.0-rc.1` needs `version = "0.1.0-rc.1"` in `Cargo.toml` and `Cargo.lock` (this corrects the transition plan, which said to bump to 0.1.0 before rc.1). The pre-release cut itself belongs to the rc plan; this plan only builds the machinery.
- `crates/sniffers/` keeps its own `0.0.1` (own workspace and lock; it leaves this repo in Phase 3). The policy script must not check it.
- Every `uses:` pinned by full commit SHA with a version comment, like the other workflows. Look the SHA up with `gh api repos/<owner>/<repo>/git/ref/tags/<tag>` at implementation time and re-verify it.
- `ci-ok` must not depend on release-please; the release-policy job may be added to its `needs`.
- First changelog entry is handwritten (`Initial release`), release-please starts from `bootstrap-sha` = the merge commit of this PR, so 800 early commits do not become a changelog.

## Review Focus

- A release PR that bumps to `1.0.0` must fail CI (test with a fixture, not by trying).
- A `Cargo.toml` bumped without `Cargo.lock` must fail CI with a message that names the fix.
- Pre-release versions (`0.1.0-rc.1`) must pass the policy script and must not move `latest` (already handled in `release.yml`; add a regression assertion in the script test).
- release-please must not touch `crates/sniffers/`.
- Non-conventional PR titles are caught before merge (squash title is the changelog input).

---

### Task 1: Policy script and CI gate

**Files:**
- Create: `.github/scripts/check_version_policy.py`, `.github/scripts/check_version_policy_test.py`
- Modify: `.github/workflows/ci.yml` (job `release-policy`, add to `ci-ok.needs`), `.github/workflows/release.yml` (run the script in `verify`)

**Interfaces:**
- Produces: `check_version_policy.py [Cargo.toml] [Cargo.lock]`; library function `check(cargo_toml_text: str, cargo_lock_text: str, allow_major: bool = False) -> str` returning the version, raising `ValueError` with the message. `allow_major` is only for tests; no env switch (a reviewed script edit is the way to 1.0).

- [ ] **Step 1: Write failing tests** (`unittest`, same style as `check_release_test.py`): `test_plain_0x_version_passes`, `test_prerelease_passes` (`0.1.0-rc.1`), `test_major_1_fails_with_policy_message`, `test_major_1_passes_with_allow_major`, `test_lock_mismatch_fails_and_names_cargo_update`, `test_lock_entries_for_non_workspace_crates_ignored` (a registry crate with another version must not matter: only lock packages with no `source` and a name in `[workspace] members` count; derive member names from the member paths' basenames, they equal crate names in this repo).
- [ ] **Step 2: Run** `python3 .github/scripts/check_version_policy_test.py`. Expected: FAIL (no module).
- [ ] **Step 3: Implement `check`** with `tomllib`; error text for the lock case: `Cargo.lock says X for wayhouse, Cargo.toml says Y: run 'cargo update --workspace' and commit Cargo.lock`.
- [ ] **Step 4: Run tests.** Expected: PASS. Then run the script on the repo. Expected: prints `0.0.1`.
- [ ] **Step 5: Wire into CI**: job `release-policy` (ubuntu-24.04, checkout, run the test then the script; no `changes` filter, it is milliseconds). In `release.yml` `verify`, run the script after `check_release_test.py`. Commit `ci: enforce the version policy and keep Cargo.lock in step (#188)`.

### Task 2: `RELEASING.md` policy sections and changelog seed

**Files:**
- Modify: `RELEASING.md` (sections `## Versioning`, `## Cutting a release`, `## Changelog`), `CONTRIBUTING.md` (conventional-commit title rule, if #187 landed; else note in this PR), `deploy/README.md` (point at RELEASING.md), `AGENTS.md` ("when you touch X" row: version or release workflow -> `RELEASING.md`, `check_version_policy.py`)
- Create: `CHANGELOG.md`

**Interfaces:**
- Consumes: `RELEASING.md` skeleton from the branching plan. If it does not exist yet, create it with the four headings.

- [ ] **Step 1: Versioning section**: single source of truth, the 0.x rules from Global Constraints, what "breaking" covers (the list), what changes at 1.0 (not decided here), where `wayhouse_build_info` and `--version` get the number.
- [ ] **Step 2: Cutting a release**: numbered runbook: (1) merge the release-please PR (title `chore(main): release 0.N.P`), (2) wait for CI on main, (3) tag `v0.N.P` on that commit and push the tag (release-please can create tags itself; we keep the tag manual so the `verify` job order stays as designed: set `skip-github-release: true` in config), (4) watch `release.yml`, (5) for a pre-release use a manual commit setting the version (`0.N.P-rc.K`) and tag it. State what to do if `publish` fails half-way (re-run; pushes are idempotent).
- [ ] **Step 3: Changelog section** plus `CHANGELOG.md` seeded with `## 0.1.0 (unreleased)` / `Initial release` and bullet areas (proxy, controller, aggregator, UI, agent, sniffers) taken from README Features.
- [ ] **Step 4: Verify** `python3 .github/scripts/check_md_links.py RELEASING.md CHANGELOG.md` if the #187 script exists. **Commit** `docs: version policy, release runbook and changelog (#188)`.

### Task 3: Conventional-commit PR title check

**Files:**
- Create: `.github/scripts/check_pr_title.py`, `.github/scripts/check_pr_title_test.py`, `.github/workflows/pr-title.yml`

**Interfaces:**
- Produces: `check_pr_title.py "<title>"` exit 0/1; regex `^(feat|fix|perf|refactor|docs|test|build|ci|chore|revert)(\([a-z0-9._-]+\))?!?: \S.*$`, max 100 chars.

- [ ] **Step 1: Failing tests**: accepts `feat: x`, `fix(controller): y`, `feat!: breaking`, `chore(main): release 0.1.0`; rejects `Add thing`, `Feat: x`, `fix:x`, empty, 101 chars.
- [ ] **Step 2: Run** `python3 .github/scripts/check_pr_title_test.py`. Expected: FAIL.
- [ ] **Step 3: Implement** and the workflow: `on: pull_request: types: [opened, edited, synchronize, reopened]`, `permissions: {}`, checkout of the base only (no PR code executed beyond the script from the base: use `pull_request_target`? No. Plain `pull_request`, script read from the PR tree, title passed through `env:`, never interpolated into `run:`).
- [ ] **Step 4: Run tests.** Expected: PASS. It is advisory (not in `ci-ok`, not required); say so in CONTRIBUTING. **Commit** `ci: check conventional-commit PR titles (#188)`.

### Task 4: release-please

**Files:**
- Create: `.github/workflows/release-please.yml`, `release-please-config.json`, `.release-please-manifest.json`

**Interfaces:**
- Consumes: `check_version_policy.py` (a release PR is just another PR, so `release-policy` guards it).
- Produces: a standing release PR on branch `release-please--branches--main`.

- [ ] **Step 1: Write `release-please-config.json`**: `{"bootstrap-sha":"<merge commit of this PR>","packages":{".":{"release-type":"simple","package-name":"wayhouse","bump-minor-pre-major":true,"bump-patch-for-minor-pre-major":true,"skip-github-release":true,"include-component-in-tag":false,"changelog-path":"CHANGELOG.md","extra-files":[{"type":"toml","path":"Cargo.toml","jsonpath":"$.workspace.package.version"}, <Cargo.lock entries>]}}}`. The `bootstrap-sha` is only known after merge: land the config in the PR with a placeholder-free approach by adding the SHA in a second tiny commit to main (`chore: set release-please bootstrap-sha`).
- [ ] **Step 2: Spike the lockfile entries.** `Cargo.lock` has one `[[package]]` per workspace member (the 10 crates in the root `Cargo.toml`), each needing `$.package[?(@.name=='<crate>')].version`. Check release-please's TOML updater accepts filter expressions by running it locally: `npx release-please@latest release-pr --dry-run --repo-url wayhouse-proxy/wayhouse --token $GITHUB_TOKEN` (needs GitHub, so do this when the outage ends). Fallback if filters are unsupported: drop the lock entries and add a workflow step after the action that checks out the release branch, runs `cargo update --workspace`, commits `Cargo.lock` as the bot and pushes; either way the `release-policy` job proves the result.
- [ ] **Step 3: Write the workflow**: `on: push: branches: [main]`; permissions `contents: write`, `pull-requests: write`, `actions: write`; step 1 the pinned action; step 2 (only when the action output `prs_created` is true or the PR exists): `gh workflow run ci.yml --ref release-please--branches--main` with `GH_TOKEN`, so `ci-ok` is reported on the release PR head. Document that fallback is a fine-grained PAT (`RELEASE_PLEASE_TOKEN`, contents and pull requests write) if dispatch checks do not satisfy the ruleset; the maintainer must create that secret, so ask rather than assume.
- [ ] **Step 4: Verify with the real run** after merge: the release PR appears, proposes `0.1.0` once the first `feat`/breaking commit exists (use `release-as: 0.1.0` in the config for the first PR only, then remove it; note this in `RELEASING.md`), `release-policy` is green, no `crates/sniffers` change in the diff. If it proposes `1.0.0`, `release-policy` must fail: confirm by reading the check.
- [ ] **Step 5: Commit** `ci: release-please opens release PRs (#188)`.

---

## Self-review

Spec coverage: lockfile check (T1), single source and rules (T2), changelog tool with 1.0 guard (T1, T4), action SHA pinning (Global Constraints). Open items needing the maintainer: the `RELEASE_PLEASE_TOKEN` secret only if dispatch does not satisfy the ruleset; verified in T4 step 4. Depends on `RELEASING.md` from the branching plan, otherwise Task 2 creates it.
