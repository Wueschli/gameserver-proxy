# CI change detection from `cargo metadata` — design

Date: 2026-10-03. Follow-up row "CI: precise change detection" in `HANDOVER.md`.

## Problem

`.github/scripts/changes.sh` decided which path-scoped CI jobs run with hand-written
regexes listing crate directories per job. They drift as crates gain dependencies: a crate
that starts depending on `wayhouse-core` would not run the jobs `wayhouse-core` changes run until
someone edits the regex. They were also coarser than the graph: a `crates/wayhouse-config/fuzz/`
change ran `plugins` and `tunnel` (it is under `crates/wayhouse-config/`), and the root
`Cargo.lock` ran `fuzz`, whose workspace has its own lockfile.

## Design

`.github/scripts/changes.py` (stdlib Python, like the other CI scripts) replaces it, with the
same interface: changed files on stdin, `--all`, `<area>=true|false` lines on stdout.

- **Graph.** `cargo metadata --no-deps --format-version 1` for every workspace in the repo
  (found from the tracked `Cargo.toml` files, shallowest first: root, `crates/plugins`,
  `crates/wayhouse-config/fuzz`). Each package is keyed by its directory, with the directories of
  its path dependencies (all kinds: normal, dev, build). `--no-deps` resolves nothing and
  downloads nothing.
- **Ownership.** A changed file belongs to the package with the longest directory prefix
  holding it. `crates/wayhouse-ui/web/` (a standalone npm project) belongs to none. A nested
  workspace's own files (`crates/plugins/Cargo.toml`, `README.md`) belong to all its members.
  A workspace's `Cargo.lock` hits only that workspace's members, not dependents elsewhere.
- **Jobs.** `ROOTS` lists what each Rust job builds or tests — `plugins`: `wayhouse` and every
  package under `crates/plugins/`; `tunnel`: the five binaries and `wayhouse-fleet-tests`;
  `fuzz`: `wayhouse-config-fuzz`. A job runs when a changed package, or any package depending on
  one transitively, is in its roots. Non-Cargo paths stay explicit (`EXTRA`): `ui`'s web dir,
  `deploy`'s paths (unchanged), `Makefile` for `tunnel`, `.config/` (nextest's `ci` profile)
  for `plugins` and `tunnel`. `release` = `plugins` or `deploy`, as before.

## Fail-open rules (never skip a job that should run)

- A `.github/` change, `--all` (schedule, dispatch, unusable diff base): everything.
- Root `Cargo.toml` (inherited by members of every workspace), `rust-toolchain.toml`,
  `.cargo/`: every Rust area.
- A path under `crates/` that no package owns (a new or not-yet-registered crate): every Rust
  area.
- `cargo metadata` or `git ls-files` failing: everything, with a `::warning::` annotation so a
  broken detector is visible rather than silently expensive.

Known blind spot: files one crate reads from another crate's directory at compile or test
time (`include_bytes!`, fixtures). Today that is only `wayhouse-fleet-tests` reading `wayhouse-http`'s
test fixtures, and `wayhouse-http` is already in every job `wayhouse-fleet-tests` is in.

## Testing

`changes_test.py` (unittest, run in the `changes` job, needs `cargo`): the old shell cases
against the real repo graph, the three behaviour changes above, the fail-open rules, and
synthetic-graph cases for the mechanism (a new dependency edge is followed transitively,
longest-prefix ownership, no partial-name matches, a lockfile not crossing workspaces).

The `changes` job runs with `RUSTUP_TOOLCHAIN=stable`, so the runner image's preinstalled
cargo serves `cargo metadata` without installing `rust-toolchain.toml`'s components.

## Out of scope

Narrowing a root `Cargo.lock` change by diffing the lockfile (nearly every real bump reaches
`wayhouse` anyway); running `deploy` on binary source changes (a cost decision, unchanged).
