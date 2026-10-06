# Releasing

How a release is cut today, and how it would be cut from a release branch later.

## Versioning

The workspace version in the root `Cargo.toml` (`[workspace.package] version`) is the single
source of truth. Every binary reports it (`--version`, the `wayhouse_build_info` metric), and
`release.yml` refuses a tag that is not `v` + that version. `crates/plugins/` has its own
workspace and keeps its own `0.0.1` until it leaves this repo.

Versions stay `0.x`. Under 0.x:

- **minor bump** (`0.N.0`): anything breaking: the config schema, CLI flags, the admin and
  fleet HTTP APIs, wire protocols, the plugin ABI, metric names, on-disk formats;
- **patch bump** (`0.N.P`): fixes and compatible additions.

Moving to 1.0 is the maintainer's explicit call, never something tooling does.
`.github/scripts/check_version_policy.py` (CI job `release-policy`, and `release.yml`)
fails any `Cargo.toml` that says `1.x` or more, and any `Cargo.lock` that disagrees with
`Cargo.toml`. Going to 1.0 means a deliberate, reviewed edit of that script (there is no environment switch).
What changes at 1.0 (the stability promise) is not decided here.

## Cutting a release

release-please (`.github/workflows/release-please.yml`) keeps one release PR open on `main`,
titled `chore(main): release 0.N.P`. It bumps `Cargo.toml`, a follow-up step refreshes
`Cargo.lock`, and the PR adds the section to `CHANGELOG.md`. Because PRs made with
`GITHUB_TOKEN` do not start workflows, the workflow dispatches `ci.yml` on the release branch
so `ci-ok` is reported; if that does not satisfy the ruleset, create a fine-grained token
secret `RELEASE_PLEASE_TOKEN` (contents and pull requests write).
The repository setting Settings > Actions > General > "Allow GitHub Actions to create and approve
pull requests" must be on, otherwise the run fails with "GitHub Actions is not permitted to create
or approve pull requests"; a `RELEASE_PLEASE_TOKEN` secret works instead of the setting.

1. Merge the release PR (check the proposed version and changelog first).
2. Wait for CI on the merge commit on `main`.
3. Tag that commit `v0.N.P` and push the tag. The tag stays manual (release-please runs with
   `skip-github-release`) so `release.yml` triggers and its `verify` job runs in order.
4. Watch `release.yml`. If `publish` fails half-way, re-run it; the pushes are idempotent.
5. **Do not forget:** on the merged release PR, change the label `autorelease: pending` to
   `autorelease: tagged`. Without it release-please silently stops opening release PRs (the
   job stays green).

The first release PR carries `"release-as": "0.1.0"` in `release-please-config.json`; remove
that line once `0.1.0` is out.

**Pre-release** (`0.N.P-rc.K`): release-please does not make these. Set the version in
`Cargo.toml` and `Cargo.lock` by hand in a normal PR (`cargo update --workspace`), merge it,
and tag `v0.N.P-rc.K`. A pre-release never moves `latest`.

## Changelog

`CHANGELOG.md` is generated from the conventional-commit titles on `main` (squash-merge title
= commit), so the PR title is the changelog entry (the repository setting "Default to pull request title" for squash merges makes that reliable): `feat:` and `fix:` appear, `docs:`, `ci:`,
`test:` and similar are hidden, `!` after the type marks a breaking change (a minor bump under
0.x). Do not edit released sections by hand. The `PR title` workflow warns about titles that
are not conventional; it is advisory, not a required check.

## Branching models

Versions stay `0.x`. Moving to 1.0 is the maintainer's decision, never something tooling does.

### Model A: trunk (default)

Feature PRs merge straight to `main` (CI plus reviewer approval, gated by `ci-ok`). `main` is
always releasable: a red `main` is fixed before anything else merges.

**Ship a release or a hotfix:**

1. Land the fix on `main` through a normal PR and wait for CI to pass.
2. Set `[workspace.package] version` in `Cargo.toml` and `Cargo.lock` to the new version
   (a new patch number for a hotfix) in a PR; merge it.
3. Wait for CI on the merge commit. For a docs-only commit CI is skipped on push, so run
   it by hand (Actions > CI > Run workflow on `main`).
4. Push the tag `v<version>` on that commit.

`release.yml` runs on the tag and refuses it unless the tag is `v` + the workspace version,
the tagged commit is on `main`, and `ci.yml` has a green run on that commit.

An old release cannot be patched in place: every fix ships as a new tag from `main`. That is
fine before 1.0.

### Model B: release branch

Used when a batch of work must land together while `main` stays releasable.

1. Cut `release/0.N` from the tag commit of `v0.N.0`.
2. Land every fix on `main` first, then copy it to the branch with `git cherry-pick -x <sha>`
   (the `-x` records where it came from). Nothing is backported automatically.
3. Tag `v0.N.P` on the branch once CI is green on that commit.

### When to switch from A to B

Switch to B when any of these holds:

- a release needs several interdependent features that must land together (e.g. a wire
  protocol or plugin ABI change spanning agent/controller/aggregator, cf. #185, #183, #184);
- `main` must stay releasable while such a batch is in flight;
- the first 1.0 maintenance branch is needed.

### What changes in the repo when switching

Nothing changes until a trigger fires. Then:

- **`release.yml`**: the "Tagged commit is on main" step accepts a commit on the matching
  release branch. Derive the branch from the tag and replace the single ancestor check:

    ```sh
    MAJOR_MINOR=$(echo "${TAG#v}" | cut -d. -f1,2)   # v0.3.2 -> 0.3
    git merge-base --is-ancestor "$SHA" origin/main ||
      git merge-base --is-ancestor "$SHA" "origin/release/${MAJOR_MINOR}" ||
      { echo "::error::$SHA is on neither main nor release/${MAJOR_MINOR}"; exit 1; }
    ```

    The job already checks out with `fetch-depth: 0`; make sure the release branches are
    fetched too.

- **Branch protection**: `ruleset-main.json` is duplicated for `release/*`, with the same
  single `ci-ok` requirement, no force push and no deletion.
- **`ci.yml`**: the `push` branch filter becomes `[main, "release/**"]`. The `pull_request`
  trigger has no branch filter, so PRs into a release branch already run.
- **`ci-ok`**: unchanged.

### Not decided

- What happens at 1.0 (branching, support window): the maintainer's call.
- Whether to publish a `release/*` ruleset before the first release branch exists: no.
