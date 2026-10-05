# v0.1.0-rc.1, Private Packages and v0.1.0 Implementation Plan (#189, #177)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans (project convention: no subagents in implementation threads). Steps use checkbox (`- [ ]`) syntax. Tasks 1 and 4 and part of Task 3 are maintainer-assisted: the thread prepares, the maintainer does the GitHub UI parts.

**Goal:** Cut `v0.1.0-rc.1` (first run of `release.yml`, which also verifies #177), keep the six GHCR packages private with documented pull instructions (#189), then ship `v0.1.0`.

**Architecture:** No new code beyond docs and a version commit. The rc is a manual commit that sets the workspace version to `0.1.0-rc.1` in `Cargo.toml` and `Cargo.lock` (the release tag must equal `v<workspace version>`), tagged by hand. `v0.1.0` comes through the release-please PR (`release-as: 0.1.0` for that first PR).

**Tech Stack:** GitHub Actions (existing `release.yml`), GHCR, Docker/buildx, Kubernetes manifests in `deploy/k8s`.

**Spec:** `2026-10-05-versioning-and-changelog.md` (policy, release-please), issues #177 and #189, transition plan Phase 2.

## Global Constraints

- **Preconditions (all must hold, check them in Task 1 step 1):** Phases 0 and 1 merged (#187, #190, #171, #188, ABI version, protocol and config versions), the GitHub Actions outage over, `make check` green and `ci.yml` green **on the exact commit** to be tagged, `crates/plugins` still bundled (the images may use them).
- Tag only from `main`. Versions never reach 1.0.0.
- Packages stay private for now (maintainer decision); no musl release assets (decision 3).
- The pre-release must not move `latest` (existing `case "$VERSION" in *-*)` in `release.yml`); verify after the run.
- A second rc is expected (`publish` has never run); each rc is `0.1.0-rc.N` with its own version commit.

## Review Focus

- `imagetools inspect` of each of the six images shows both `linux/amd64` and `linux/arm64` and **the per-arch digests equal those of the `-amd64` and `-arm64` tags** (the #177 check), not just a manifest list that exists.
- An anonymous `docker pull` of each image fails; a pull with a `read:packages` token works (the #189 acceptance).
- `docker run --platform linux/arm64 ghcr.io/<owner>/wayhouse:0.1.0-rc.1 --version` prints `0.1.0-rc.1 (<commit>)` with the tagged commit.
- Package-to-repo linking (`org.opencontainers.image.source` label is set by `release.yml`, which links the package to the repository and lets `GITHUB_TOKEN` publish).

---

### Task 1: Prepare the rc commit and checklist

**Files:**
- Modify: `Cargo.toml`, `Cargo.lock`, `CHANGELOG.md`, `README.md` (caution block wording: "first pre-release available" only after the tag exists, so this edit lands in Task 5, not here)

- [ ] **Step 1: Verify preconditions.** `git log origin/main -1`; GitHub status check (outage over); `gh run list --workflow ci.yml --branch main --limit 3` shows success for the HEAD commit; `python3 .github/scripts/check_version_policy.py`. Expected: all green. If not, stop and report.
- [ ] **Step 2: Version commit.** Set `[workspace.package] version = "0.1.0-rc.1"`, run `cargo update --workspace` (only workspace entries change; check `git diff --stat Cargo.lock` touches only the 10 member versions), run `python3 .github/scripts/check_version_policy.py` (expected: prints `0.1.0-rc.1`) and `python3 .github/scripts/check_release.py v0.1.0-rc.1` (expected: prints `0.1.0-rc.1`), then `make check`.
- [ ] **Step 3: Open the PR** `chore: release 0.1.0-rc.1`, wait for `ci-ok`, merge to main.
- [ ] **Step 4: Confirm CI is green on the merge commit**, then proceed. Docs-only shortcuts do not apply: this commit changes `Cargo.*`.

### Task 2: Tag and run `release.yml`, verify #177

- [ ] **Step 1: Tag** `git tag -a v0.1.0-rc.1 -m "v0.1.0-rc.1" <merge sha> && git push origin v0.1.0-rc.1` (the maintainer or a thread with tag push rights; threads cannot delete remote branches, so confirm rights first or ask the maintainer to push the tag).
- [ ] **Step 2: Watch** `verify`, `images` (amd64 and arm64) and `publish` (`mcp__github__actions_list` / `actions_get`); a failure before `publish` leaves nothing public. Collect logs with `get_job_logs` for any failure.
- [ ] **Step 3: Run the #177 checks** on a machine with Docker and a `read:packages` token: `docker login ghcr.io`; for each of the six images `docker buildx imagetools inspect ghcr.io/<owner>/<image>:0.1.0-rc.1` and compare digests with `...:0.1.0-rc.1-amd64` and `...-arm64`; `docker run --rm --platform linux/arm64 ghcr.io/<owner>/wayhouse:0.1.0-rc.1 --version`. Record the output in a comment on #177. Expected: both platforms present, digests equal, version string right.
- [ ] **Step 4: If it fails**, apply the fallback written in #177 (push per-arch tags from the native runner, keep only `imagetools create` in `publish`), fix forward as `0.1.0-rc.2` (repeat Tasks 1 and 2 with the new number). Record what failed in `HANDOVER.md`.
- [ ] **Step 5: Check `latest` was not moved** (the package page or `docker buildx imagetools inspect ...:latest` must not exist yet).

### Task 3: Private packages (#189)

**Files:**
- Modify: `deploy/README.md` (new section "Pulling private images"), `deploy/k8s/*.yaml` (add `imagePullSecrets` stanza, commented example), `deploy/compose/README` or compose docs (login note), `docs/12-deployment.md` (one paragraph and link)

- [ ] **Step 1: Maintainer check (manual).** In GitHub, Packages: all six (`wayhouse`, `wayhouse-minimal`, `wayhouse-controller`, `wayhouse-aggregator`, `wayhouse-ui`, `wayhouse-agent`) exist, are **private**, and are linked to the repository (the page shows the repository link; a public repo does not make packages public). Maintainer confirms in the thread.
- [ ] **Step 2: Write the docs**: `docker login ghcr.io -u <user> --password-stdin` with a classic PAT with `read:packages` (state the scope exactly and that fine-grained PATs do not work for GHCR; verify this claim against the current GitHub docs before writing it), the Kubernetes variant (`kubectl create secret docker-registry ghcr --docker-server=ghcr.io ...` and `imagePullSecrets: [{name: ghcr}]` in each workload), the compose variant, and a "make public later" checklist (flip visibility on six packages, delete the token notes, announce).
- [ ] **Step 3: Acceptance test (manual, recorded)**: anonymous `docker pull ghcr.io/<owner>/wayhouse:0.1.0-rc.1` fails with `unauthorized`/`denied`; with the token it succeeds. Paste both outputs in the PR.
- [ ] **Step 4: Run** `make deploy-lint` (k8s manifests still valid). **Commit** `docs: pulling the private images, how to make them public later (#189)`.

### Task 4: Soak and v0.1.0

- [ ] **Step 1: Soak** the rc for a few days: run the compose demo from the pulled images, `make deploy-smoke`; file anything found as issues. No code needed in this plan for that.
- [ ] **Step 2: Remove `release-as`'s need**: let release-please open the PR whose title is `chore(main): release 0.1.0` with `release-as: 0.1.0` set in the config for this first PR. Confirm the PR's diff: `Cargo.toml`, `Cargo.lock`, `CHANGELOG.md`, `.release-please-manifest.json`, nothing else; `release-policy` green; `ci-ok` reported (dispatch workaround from the versioning plan).
- [ ] **Step 3: Merge, wait for CI on main, tag `v0.1.0`**, watch `release.yml`; verify `latest` now points at `0.1.0` for all six; re-run the Task 2 step 3 checks for `0.1.0`.
- [ ] **Step 4: Cleanup commit**: remove `release-as` from `release-please-config.json`; README caution block wording ("the first release exists, private packages, still 0.x and subject to breaking changes"); `HANDOVER.md` state; `CHANGELOG.md` is already updated by release-please. Remove "make the 6 packages public" from the maintainer's manual list in project memory.
- [ ] **Step 5: Commit** `docs: v0.1.0 is out (#189)`.

---

## Self-review

Spec coverage: #177 check and fallback (T2), #189 verification, docs and acceptance (T3), rc to v0.1.0 flow (T4). Correction recorded: rc.1 needs its own version commit because `check_release.py` enforces tag equals workspace version. Needs the maintainer for: tag push rights (maybe), package visibility check, the PAT, the GitHub UI parts.
