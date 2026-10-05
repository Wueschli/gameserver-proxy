# Releasing

How a release is cut today, and how it would be cut from a release branch later.

## Versioning

(Filled in by the versioning and changelog work, #188.)

## Cutting a release

(Filled in by #188.)

## Changelog

(Filled in by #188.)

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
