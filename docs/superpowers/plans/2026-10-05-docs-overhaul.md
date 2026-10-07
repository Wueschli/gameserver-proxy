# Docs Overhaul Implementation Plan (#187)

> **Terminology update (2026-10-07).** Leandro split the old "plugin" concept in two. **Sniffers** are the WASM protocol/hostname sniffer modules and live in [`wayhouse-proxy/sniffers`](https://github.com/wayhouse-proxy/sniffers). **Plugins** are integrations with other systems (e.g. the Pelican panel, #213) and live in [`wayhouse-proxy/plugins`](https://github.com/wayhouse-proxy/plugins); their design is still open. Wherever this document says "plugin" or "plugins repo" for a WASM sniffer module, read **sniffer** / **sniffers repo**. Code identifiers, crates and paths were renamed to "sniffer" in the same PR as this banner; older text below may still show the old names.


> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans (project convention: no subagents in implementation threads). Steps use checkbox (`- [ ]`) syntax.

**Goal:** A user-facing `README.md`, a contributor-facing `CONTRIBUTING.md`, and Prettier-checked Markdown, with contributor-only text moved out of the files users read first.

**Architecture:** Split by audience. `README.md` = what it is, install, minimal config, links. `CONTRIBUTING.md` = build, `make check`, conventions, PR flow. `AGENTS.md` keeps the agent working agreement and links to `CONTRIBUTING.md`; `HANDOVER.md` loses contributor boilerplate. Prettier runs on Markdown via a `make docs-fmt-check` target and a CI job gated by the `changes` filter.

**Tech Stack:** Markdown, Prettier (pinned version, via `npx --yes prettier@<pinned>`), GitHub Actions.

**Spec:** issue #187; `docs/superpowers/plans/2026-10-05-transition-index.md`.

## Global Constraints

- README keeps the WIP caution block until v0.1.0 is tagged, then #189 and the release plan reword it. Do not claim "no releases" after the tag (the rc plan edits it).
- Install section waits for #188/#189: write it with a clearly marked `<!-- install: filled by the rc.1 plan -->` placeholder that names source build only, no GHCR claims.
- Do not touch `docs/00-12*.md` content except link fixes.
- Prettier must not reflow code blocks or tables in a way that breaks them; use `proseWrap: preserve`.
- Docs-only PR, merges on green CI. Existing conventions: specs in `docs/superpowers/specs`, plans in `docs/superpowers/plans`.

## Review Focus

- A new reader must reach "build and run with the example config" in README within the first screen of Quick start, without reading contributor material.
- Every relative link in the touched files must resolve (a link checker step is part of Task 3).
- Prettier must not rewrite generated or vendored Markdown (`target/`, `node_modules/`, `crates/**/node_modules`).

---

### Task 1: Split README (users) from CONTRIBUTING (contributors)

**Files:**
- Modify: `README.md` (263 lines today), `AGENTS.md`, `HANDOVER.md`, `deploy/README.md` (link only)
- Create: `CONTRIBUTING.md`

**Interfaces:**
- Produces: `CONTRIBUTING.md` headings `## Build and test`, `## Conventions`, `## Pull requests`, `## Releasing` (one link to `RELEASING.md`), referenced by README and AGENTS.

- [ ] **Step 1: Inventory.** Run `grep -n "^#" README.md AGENTS.md HANDOVER.md` and write a table in the PR description: each section, audience (user / contributor / agent), destination. Sections about `make`, TDD, repo layout, conventions are contributor material.
- [ ] **Step 2: Create `CONTRIBUTING.md`** by moving (not copying) the contributor sections of README, keeping `AGENTS.md`'s agent-only guardrails where they are. Content: toolchain (`rust-toolchain.toml`, protoc), `make check`, `make plugins` (until the sniffers move), commit and PR title format (conventional commits, which #188 needs), `Closes #N`, review and CI rules, where specs and plans live.
- [ ] **Step 3: Rewrite README sections**: Features, Quick start (build from source, run `config.example.yaml`, the admin port), Configuration pointer to `docs/05-configuration.md`, Sniffers pointer, License; links to `CONTRIBUTING.md`, `RELEASING.md` (exists after #190), `docs/`.
- [ ] **Step 4: Trim AGENTS.md and HANDOVER.md**: replace duplicated contributor text by links to `CONTRIBUTING.md`; keep the "when you touch X, also touch Y" table in AGENTS.md. Add `CONTRIBUTING.md` and `RELEASING.md` to that table's rows for "release/version changes".
- [ ] **Step 5: Link check.** Run `python3 .github/scripts/check_md_links.py README.md CONTRIBUTING.md AGENTS.md HANDOVER.md RELEASING.md deploy/README.md` (created in Task 3; for now check by hand with `grep -on "](\.[^)]*)"`). Expected: every target exists.
- [ ] **Step 6: Commit** `docs: split the README for users from CONTRIBUTING for contributors (#187)`.

### Task 2: Prettier for Markdown

**Files:**
- Create: `.prettierrc.json` (`{"proseWrap":"preserve","printWidth":100}`), `.prettierignore` (`target/`, `node_modules/`, `crates/*/node_modules/`, `docs/superpowers/**` for the first cut so existing plans are not churned)
- Modify: `Makefile` (targets `docs-fmt` and `docs-fmt-check`: `npx --yes prettier@3.6.2 --check "**/*.md"`; pick the then-current 3.x at implementation time and pin it exactly)

**Interfaces:**
- Produces: `make docs-fmt-check` (exit 0 when clean), used by Task 3.

- [ ] **Step 1: Run `make docs-fmt-check`** after adding the config. Expected: FAIL listing unformatted files.
- [ ] **Step 2: Run `make docs-fmt`** (applies `--write`), review `git diff --stat` for table/code damage; if a table breaks, add `<!-- prettier-ignore -->` above it.
- [ ] **Step 3: Re-run `make docs-fmt-check`.** Expected: PASS.
- [ ] **Step 4: Commit** `docs: format Markdown with Prettier (#187)`.

### Task 3: CI job and link checker

**Files:**
- Create: `.github/scripts/check_md_links.py` and `.github/scripts/check_md_links_test.py`
- Modify: `.github/workflows/ci.yml` (new job `docs`, added to `ci-ok` `needs`), `.github/scripts/changes.py` (+ `changes_test.py`: a `docs` output true for `*.md`, `.prettierrc.json`)

**Interfaces:**
- Produces: `check_md_links.py <files...>` exits 1 and prints `file:line: broken link target` for each relative link whose file or `#anchor` heading is missing; job output `needs.changes.outputs.docs`.
- Consumes: `make docs-fmt-check` from Task 2.

- [ ] **Step 1: Write `check_md_links_test.py`** with cases `test_existing_relative_link_passes`, `test_missing_file_fails`, `test_missing_anchor_fails` (anchors are GitHub slugs: lowercase, spaces to `-`, punctuation dropped), `test_external_and_mailto_links_ignored`, `test_links_inside_code_fences_ignored`.
- [ ] **Step 2: Run** `python3 .github/scripts/check_md_links_test.py`. Expected: FAIL (module missing).
- [ ] **Step 3: Implement `check_md_links.py`** (stdlib only, same style as `check_release.py`).
- [ ] **Step 4: Run test.** Expected: PASS. Then run it on the real docs; fix any broken links it finds.
- [ ] **Step 5: Add the `docs` job** to `ci.yml` running Prettier check and the link check, `if: needs.changes.outputs.docs == 'true'`, runner `ubuntu-24.04`, actions pinned by SHA like the others (copy the checkout line); add `docs` to `ci-ok` `needs`. Extend `changes.py` and its test.
- [ ] **Step 6: Run** `python3 .github/scripts/changes_test.py`. Expected: PASS. **Commit** `ci: check Markdown formatting and links (#187)`.

---

## Self-review

Spec coverage: user README, CONTRIBUTING, prettier covered; install section is deferred by design (decision: README install after #188/#189). Task 3 touches `ci-ok`, so it merges after #179 (already on main).
