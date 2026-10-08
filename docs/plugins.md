# Plugins

A **plugin** is a WASM module that reacts to a trigger and acts through capabilities
the operator approved (design: [plugin system](superpowers/specs/2026-10-07-plugin-system-design.md)
and [automation hooks](superpowers/specs/2026-10-07-plugin-automation-hooks-design.md)).
Plugins are not [sniffers](sniffers.md): sniffers run on the proxy data path.

**Status.** The host runtime is built (`crates/wayhouse-plugin-host`); it is not wired into
the controller yet, so nothing runs a plugin today. The controller host, the install API,
secrets, `http`, `routes`, webhooks, events and `backends` are later slices.

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
