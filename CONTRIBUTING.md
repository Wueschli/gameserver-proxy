# Contributing

Thanks for helping. This is the short path from a clean checkout to a merged pull request.
Agents follow [`AGENTS.md`](AGENTS.md) on top of this; current state and open decisions are in
[`HANDOVER.md`](HANDOVER.md).

## Build and test

You need the Rust toolchain from `rust-toolchain.toml` (stable; CI and the maintainers use rustc 1.99, install with `rustup`) and
`protoc` on `PATH` (`apt install protobuf-compiler` or `brew install protobuf`), because
`crates/wayhouse/build.rs` generates the gRPC resolver client.

```sh
export PATH="$HOME/.cargo/bin:$PATH"   # if cargo is not on PATH
make check                             # fmt check + clippy (-D warnings) + tests
```

Run `make check` before every commit. Run `cargo fmt --all` (the writing form) as its own step
first: `make check` only reports formatting diffs, so an early fmt pass does not cover later
edits.

Other targets (`make help` lists all):

| Target                     | What it does                                                         |
| -------------------------- | -------------------------------------------------------------------- |
| `make run`                 | run the proxy against `config.example.yaml`                          |
| `make sniffers-fetch`      | download the pinned official sniffers for the e2e tests (network)    |
| `make ui` / `make ui-test` | build / test the admin GUI frontend (needs Node and npm)             |
| `make bench` / `make fuzz` | latency harness / parser fuzzing (fuzz needs nightly and cargo-fuzz) |
| `make docs-fmt-check`      | check Markdown formatting (needs Node; see below)                    |

The full command table (tunnel e2e, deploy images, audit, and so on) is in
[`AGENTS.md`](AGENTS.md#commands).

### Repository layout

| Crate                                                        | Responsibility                                                                    |
| ------------------------------------------------------------ | --------------------------------------------------------------------------------- |
| [`crates/wayhouse-config`](crates/wayhouse-config)           | YAML config types, parsing and validation                                         |
| [`crates/wayhouse-core`](crates/wayhouse-core)               | Data plane: config snapshot, backend pools, TCP and UDP listeners, session tables |
| [`crates/wayhouse`](crates/wayhouse)                         | The proxy binary: CLI, admin API, controller/aggregator/tunnel clients            |
| [`crates/wayhouse-controller`](crates/wayhouse-controller)   | Config and intent distribution (revision store, SSE, Raft HA, canary rollout)     |
| [`crates/wayhouse-aggregator`](crates/wayhouse-aggregator)   | Fleet-state fan-in and intent fan-out                                             |
| [`crates/wayhouse-ui`](crates/wayhouse-ui)                   | Admin GUI backend plus the React/Vite/TS frontend in `web/`                       |
| [`crates/wayhouse-agent`](crates/wayhouse-agent)             | Origin-side WireGuard agent                                                       |
| [`crates/wayhouse-http`](crates/wayhouse-http)               | Shared HTTP client and TLS server helpers                                         |
| [`crates/wayhouse-fleet-tests`](crates/wayhouse-fleet-tests) | Multi-process integration tests over the real binaries                            |
| [`crates/wayhouse-bench`](crates/wayhouse-bench)             | Latency / load harness                                                            |
| [`crates/sniffer-abi`](crates/sniffer-abi)                   | Guest-side ABI crate the official sniffers link (they live in the sniffers repo)  |

## Conventions

- **Tests first.** Write a failing test, watch it fail, then make it pass. Unit tests live in the
  module; end-to-end forwarding tests in `crates/wayhouse-core/tests/`. Tests may use `127.0.0.1:0`
  sockets and no other network.
- **Latency first.** Weigh every design choice against added round-trip time. If you add a task or
  a hop per connection, say so in the PR.
- **Agnostic core.** Game-specific knowledge lives only in sniffers or an external resolver.
- **Pre-1.0: clean breaks over shims.** When the config schema changes, update the
  `wayhouse-config` types and `validate()`, `config.example.yaml` and `docs/05-configuration.md`.
  [`AGENTS.md`](AGENTS.md#when-you-touch-x-also-touch-y) lists what else to touch for other changes.
- **Design first for larger work.** Write a spec in `docs/superpowers/specs/` and a plan in
  `docs/superpowers/plans/` before the code.
- **Commits and PR titles** use conventional commits (`feat:`, `fix:`, `docs:`, `ci:`, ...); the
  squash title is the changelog entry (release-please, see [`RELEASING.md`](RELEASING.md)). The
  `PR title` workflow flags titles that do not parse; it is advisory, not required.
- **Markdown** is formatted with Prettier: `make docs-fmt` writes, `make docs-fmt-check` verifies.

## Pull requests

1. Branch from `main`; one topic per PR.
2. Link the issue: `Closes #N` when the PR finishes it, `Part of #N` otherwise.
3. Assign an issue to yourself only when you actually start work on it, so others can pick up
   unstarted ones.
4. Push, open the PR and wait for CI. The required check is `ci-ok`; a docs-only change still
   runs it, with the heavy jobs skipped.
5. Merge once `ci-ok` is green and a reviewer approved. Docs-only PRs merge on green CI.
   A push after approval that changes more than docs needs a new review.

CI runs are slow (about 10 minutes warm), so batch your pushes.

## Releasing

See [`RELEASING.md`](RELEASING.md#branching-models) for how releases are cut and when to move to
a release branch.
