# Release Branching Doc Implementation Plan (#190)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans (project convention: no subagents in implementation threads). Steps use checkbox (`- [ ]`) syntax.

**Goal:** Document how releases are cut from trunk today (Model A) and when and how to switch to a release branch (Model B), so the decision is written down before it is needed.

**Architecture:** Docs only. Creates `RELEASING.md` with a "Branching models" section; #188 (versioning plan) appends the version-policy sections to the same file later. No workflow change is made; the doc names what *would* change.

**Tech Stack:** Markdown. Prettier is introduced by #187; until then keep lines under 100 columns.

**Spec:** issue #190; `docs/superpowers/plans/2026-10-05-transition-index.md` (order and decisions).

## Global Constraints

- Versions stay `0.x`; the doc must say 1.0 is a decision for the maintainer, not something tooling does.
- Merge directly to `main` stays the default model until a trigger below fires.
- `release.yml` today requires: tag `v<workspace version>`, tagged commit on `main` (`git merge-base --is-ancestor "$SHA" origin/main`), a green `ci.yml` run on that SHA.
- The `protect-main` ruleset (`/mnt/project-files/wayhouse/ruleset-main.json`) requires only `ci-ok`.
- Docs-only PR: merges on green CI. Commit trailer and PR body block per the session attribution rules.

## Review Focus

- A reader who only has a hotfix to ship must find the steps in under a minute (a numbered list, not prose).
- The doc must not promise tooling that does not exist (no "automatically backports").
- The Model B `release.yml` change must be spelled out exactly, because the current check rejects any commit not on `main`.

---

### Task 1: Write `RELEASING.md` branching section

**Files:**
- Create: `RELEASING.md`
- Modify: `deploy/README.md` (the "Releasing" paragraph: replace the inline recipe by a link to `RELEASING.md`, keep one sentence)

**Interfaces:**
- Produces: `RELEASING.md` with top-level headings `# Releasing`, `## Branching models` (this plan), and reserved empty headings `## Versioning`, `## Cutting a release`, `## Changelog` that #188 fills. Anchors `#branching-models` is linked from `CONTRIBUTING.md` (#187).

- [ ] **Step 1: Create `RELEASING.md`** with the four headings above and under `## Branching models`: (a) "Model A: trunk (default)": all releases are tags on `main`; hotfix = fix on `main`, new patch tag; the rule that main is always releasable (CI green); (b) "Model B: release branch": branch name `release/0.N` cut from the tag commit of `0.N.0`; fixes land on `main` first and are cherry-picked (`git cherry-pick -x`) to the branch; tags `v0.N.P` are cut on the branch; (c) "When to switch A to B", the trigger list copied from issue #190 verbatim (open the issue and copy; do not paraphrase); (d) "What changes in the repo when switching": `release.yml` verify step changes the ancestor check to `git merge-base --is-ancestor "$SHA" "origin/main" || git merge-base --is-ancestor "$SHA" "origin/release/${MAJOR_MINOR}"` (show the exact lines and the `MAJOR_MINOR` derivation from the tag), `ruleset-main.json` is duplicated for `release/*` with the same `ci-ok` requirement, `ci.yml` `push`/`pull_request` branch filters gain `release/**`, and `ci-ok` is unchanged.
- [ ] **Step 2: Add a "Not decided" list**: what happens at 1.0 (not decided, maintainer's call), whether to publish a `release/` ruleset before the first branch exists (no).
- [ ] **Step 3: Update `deploy/README.md`**: replace the recipe under "Releasing." by one sentence plus the link `[RELEASING.md](../RELEASING.md)`.
- [ ] **Step 4: Verify links**: `grep -n "RELEASING.md" -r README.md AGENTS.md deploy docs | head`; run `python3 -c "import pathlib,re;t=pathlib.Path('RELEASING.md').read_text();assert t.count('\n## ')>=4"`. Expected: no error.
- [ ] **Step 5: Commit** `docs: RELEASING.md with the trunk and release-branch models (#190)`.

---

## Self-review

Spec coverage: both models, trigger list, workflow/ruleset/`ci-ok` impact all land in Task 1. One task is intentional: nothing here is independently reviewable.
