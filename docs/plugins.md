# Plugins

A **plugin** is a WASM module that reacts to a trigger and acts through capabilities
the operator approved (design: [plugin system](superpowers/specs/2026-10-07-plugin-system-design.md)
and [automation hooks](superpowers/specs/2026-10-07-plugin-automation-hooks-design.md)).
Plugins are not [sniffers](sniffers.md): sniffers run on the proxy data path.

**Status.** The host runtime is built (`crates/wayhouse-plugin-host`) and a standalone
controller can store plugin installs behind an admin API and ticks the enabled ones
(below); the web UI has a Plugins page on top of the install API (see "The Plugins page"). Not built yet: the HA leader tick with a term-checked commit and replicated
install state, secrets, `http`, `routes`, webhooks, events and `backends`.

## Module contract (ABI 0.1)

Link `wayhouse-plugin-abi` (it stamps the `wayhouse.plugin-abi` section and exports
`alloc`). The host accepts a module only when the section equals its own version (exact
minor match while the major is 0), and reads the section before compiling anything.

A second custom section, `wayhouse.plugin-caps`, holds the capabilities as JSON. It is read
without running the module and approvals are recorded against the module's sha256:

```json
{
    "triggers": { "on_timer": true },
    "tick_interval_secs": 30,
    "log": true,
    "state": { "max_bytes": 65536 }
}
```

Unknown fields are an error. `tick_interval_secs` is required with `on_timer` and at
least 10.

Exports: `memory`, `alloc(i32) -> i32`, `init(config_ptr, config_len)`, and `on_timer()` when
declared. The only imports allowed are these, in the `wayhouse` namespace:

| Import                                           | Capability | Behaviour                                                                                                                        |
| ------------------------------------------------ | ---------- | -------------------------------------------------------------------------------------------------------------------------------- |
| `log(level, ptr, len)`                           | `log`      | levels 0 error, 1 warn, 2 info, else debug; lines over 1024 bytes are cut; lines past the per-call limit are dropped and counted |
| `state_get(kptr, klen, out_ptr, out_cap) -> i32` | `state`    | value length, or -1 when absent; nothing is written when the value is longer than `out_cap`                                      |
| `state_put(kptr, klen, vptr, vlen) -> i32`       | `state`    | 0, or -1 when over `state.max_bytes`, the key is over 256 bytes or not UTF-8, or the per-call write limit is hit                 |

A call that uses a capability it did not declare traps that call (`CallError::CapabilityDenied`)
and leaves the host running.

## Approval

`PluginHost::load(bytes, approved)` takes the capabilities the operator approved for the
module's sha256. A module whose own declaration asks for more (a new trigger, `log`,
`state`, or a bigger state cap) fails with `CapsNotApproved`, so a module cannot grant
itself anything. It then runs with its own declaration, which is never wider than the
approved set.

## Limits

Every call runs in a fresh `Store` with a memory cap, an epoch deadline
(`Limits::call_timeout`), a table-element cap (`Limits::max_table_elements`, checked at instantiation and on growth) and the log and state-write limits above. Modules are capped at
8 MiB. A call returns its `Effects` (state writes, log lines) and applies nothing itself:
the caller commits them as one entry, which is how the controller will tag them with the
leader term (automation hooks spec, "Commit rule").

## Compile bounds and the compile pool

Before a module is compiled, `bounds::check` walks it with `wasmparser` and rejects
structure out of proportion to a real plugin (`ModuleError::TooComplex`, naming the
bound): functions, types, locals per function, block nesting depth, declared table and
memory sizes, globals, imports, exports and total code bytes (`Bounds`, set through
`Limits::bounds`). `CompilePool` runs `PluginHost::load` on a fixed set of worker threads
behind a bounded queue: a full queue answers `Busy` at once and a slow compile answers
`TimedOut` (the compile itself cannot be interrupted, so it keeps its worker until done).
The controller must load modules through the pool, never on its main loop. The workspace sets `panic = "abort"` for the release profile, so a panic in a compile worker (for example a wasmtime bug) ends the process rather than silently shrinking the pool; the bounds exist to keep hostile input away from that path.

A declared `state.max_bytes` above 1 MiB is rejected, and `PluginHost::new` refuses a zero
`call_timeout`.

## Checking a built plugin

`wayhouse-plugin-check <module.wasm>` (crate `wayhouse-plugin-host`) loads the module with
its own declaration as the approved set, calls `init` and, if declared, `on_timer` once,
and prints what the host saw, including which capabilities were used. It exits 1 when the
host would reject the module, a call fails, or the plugin used a capability it did not
declare. Plugin repo CI is meant to run it on every build.

## Installing a plugin (controller API)

Start a standalone controller with `--plugins` (off by default). With `--ha-peers`, with
`--role slave`, or without the flag, `/plugins` answers `501` with the reason. The routes sit
behind the same admin bearer token as `/config`; `X-Actor` is recorded as `created_by`.

| Request                                                       | What it does                                                                                                                                                                                                                                         |
| ------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `POST /plugins/modules` (raw `.wasm` body, up to 8 MiB)       | Validates ABI and capability sections, keeps the bytes by sha256, returns `{sha256, size, abi, capabilities}` for the operator to read.                                                                                                              |
| `POST /plugins` `{name, sha256, approved, config?, enabled?}` | Compiles the stored module through the bounded pool with `approved` as the operator-approved set; `422` when the module declares more than `approved` or fails a bound, `503` when the compiler is busy. Records the install and returns it (`201`). |
| `GET /plugins`, `GET /plugins/{id}`                           | The install records.                                                                                                                                                                                                                                 |
| `POST /plugins/{id}/enable`, `/disable`                       | Flip `enabled`; a disabled install is not ticked, and enabling it restarts its first-tick wait.                                                                                                                                                      |
| `GET /plugins/{id}/status`                                    | What the ticks did: `ticks`, `last_tick_unix`, `last_ok`, `last_error`, `consecutive_failures` and the last 100 log lines. In memory only; empty before the first tick.                                                                              |
| `DELETE /plugins/{id}`                                        | Removes the install; the module blob goes with the last install that uses it.                                                                                                                                                                        |

An install has a random id (route ownership will be `plugin:<id>`), a name (`a-z`, `0-9`, `-`, up
to 64 characters), the approved capability set bound to the module sha256, and a non-secret
`config` (up to 64 KiB) that will be handed to `init`. Installs and blobs live in the
controller's `sled` database; they are not replicated.

## Ticks (standalone controller)

A scan runs once a second. An enabled install whose approved declaration has `on_timer`
is called every `tick_interval_secs`; the first tick comes one full interval after the
controller (or the enable) first sees the install, so enabling never runs a plugin inside
the request. The module is compiled on first use through the bounded pool, `init` runs
with the install's `config` each time the module is loaded (first tick after a controller
restart, after disable then enable, and after a failed load) against the state already stored,
so it must tolerate existing state, and every call runs on the pool: a busy or slow pool
skips the tick (shown in the status) and never blocks the controller; a skipped tick is retried at the next scan, not a whole interval later. One call per
install is in flight at a time.

A call returns its state writes; the controller applies them as one batch only if the
install's state revision is still the one the call read (compare-and-set), then bumps the
revision. A call that failed (trap, timeout, over a limit) commits nothing, is recorded in
the status, and the plugin is called again at its next interval. Deleting an install
removes its state. This is the single-node form of the design's term-checked commit; HA
controllers answer `501` until the replicated slice.

## The Plugins page (web UI)

`wayhouse-ui` proxies the controller's `/plugins` API (`/api/plugins*`, needs `--controller-url`). Listing is viewer-level; upload, install, enable, disable and delete need the admin role. The page lists installs with their approved capabilities in plain words. "Upload plugin" sends the module (up to 8 MiB) to the controller, shows what it declares and installs it only after the operator ticks the approval box; the UI approves exactly the declared set, never an edited one. A controller that does not serve plugins (off, HA, slave) shows its reason instead of a list. Not in the page yet: registry install, secrets, config editing.

## High availability (in progress)

The HA state machine already applies plugin entries, so every replica of a tier holds the
same installs and the same per-install state, in the log and in snapshots (module blobs
are not in either). The entries are an install (refused if the id exists), enable or
disable, delete (state goes with it), and one state commit per plugin call, which applies
only if the install's state revision is still the one the call read and the entry was
appended in the term the leader proposed it in. Nothing reaches this yet: an HA
controller answers `501` on `/plugins` until the next slices serve the API through the
leader, run the tick runner on the leader only, and replicate module bytes between
controllers. A cluster that uses plugins must run one build on all replicas; the
rolling-upgrade gating of new entry types follows the component versioning work.
