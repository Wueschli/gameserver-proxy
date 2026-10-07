# Sniffers Repo Push Check Implementation Plan (#183 prerequisite)

> **Terminology update (2026-10-07).** Leandro split the old "plugin" concept in two. **Sniffers** are the WASM protocol/hostname sniffer modules and live in [`wayhouse-proxy/sniffers`](https://github.com/wayhouse-proxy/sniffers). **Plugins** are integrations with other systems (e.g. the Pelican panel, #213) and live in [`wayhouse-proxy/plugins`](https://github.com/wayhouse-proxy/plugins); their design is still open. Wherever this document says "plugin" or "plugins repo" for a WASM sniffer module, read **sniffer** / **sniffers repo**. Code identifiers, crates and paths were renamed to "sniffer" in the same PR as this banner; older text below may still show the old names.


> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Prove that a Claude thread can push to the empty `wayhouse-proxy/sniffers` repository, before Phase 3 depends on it.

**Architecture:** One tiny commit to an empty repository: a README that says what the repo will hold. The repo is empty, so the GitHub contents API answers 409; the first commit has to go through `git push` (or the contents API `create_or_update_file`, which can create the initial commit).

**Tech Stack:** git, GitHub MCP tools.

**Spec:** `/mnt/project-files/wayhouse/transition-plan.md` section 3, row "Now (Phase 0)".

## Global Constraints

- The thread must have `wayhouse-proxy/sniffers` in scope (it is listed for this project).
- No sniffer moves here. The README is placeholder text only and must not promise a registry format (the manifest format is frozen only by the manifest plan).
- Branch `claude/project-thread-<id>` is the push target for threads; the very first commit on an empty repo must also create the default branch. Ask the maintainer once whether the default branch should be `main` and push the first commit there only if they say so; otherwise push to the thread branch and open a PR (a PR cannot target a non-existent base: if so, report that the maintainer must create the initial commit through the GitHub UI "Add a README" button, which is the fallback).

## Review Focus

- Success is a commit visible in the repo, not an absence of an error; verify by reading it back.
- Do not leave a stray branch or file that the later bootstrap has to clean up.

---

### Task 1: Push one commit

- [ ] **Step 1: Check the state.** `git ls-remote https://github.com/wayhouse-proxy/sniffers` (empty output means no refs). Expected: empty.
- [ ] **Step 2: Try the API route first.** `mcp__github__create_or_update_file` with `path: README.md`, a three-line body (`# wayhouse plugins`, one sentence "Community sniffers for wayhouse. Not yet populated; see the wayhouse repository, issue #183.", the at-your-own-risk note), `branch: main`, message `docs: placeholder README`. Expected: a commit sha. If it errors, continue to Step 3.
- [ ] **Step 3: Fall back to git.** In a scratch clone: `git init`, add the README, `git remote add origin ...`, `git push -u origin HEAD:refs/heads/main`. Expected: success or a precise permission error.
- [ ] **Step 4: Verify.** `mcp__github__get_file_contents` on `README.md`. Expected: the content.
- [ ] **Step 5: Report** the outcome (works / does not, with the error) on issue #183 as a comment, and in the thread. If push fails, say what the maintainer must change (app installation scope for the repository).

---

## Self-review

One task by design; the outcome gates Phase 3 only. Not a PR in the wayhouse repo.
