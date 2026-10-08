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
{"triggers": {"on_timer": true}, "tick_interval_secs": 30, "log": true, "state": {"max_bytes": 65536}}
```

Unknown fields are an error. `tick_interval_secs` is required with `on_timer` and at
least 10.

Exports: `memory`, `alloc(i32) -> i32`, `init(config_ptr, config_len)`, and `on_timer()` when
declared. The only imports allowed are these, in the `wayhouse` namespace:

| Import | Capability | Behaviour |
|--------|------------|-----------|
| `log(level, ptr, len)` | `log` | levels 0 error, 1 warn, 2 info, else debug; lines over 1024 bytes are cut; lines past the per-call limit are dropped and counted |
| `state_get(kptr, klen, out_ptr, out_cap) -> i32` | `state` | value length, or -1 when absent; nothing is written when the value is longer than `out_cap` |
| `state_put(kptr, klen, vptr, vlen) -> i32` | `state` | 0, or -1 when over `state.max_bytes`, the key is over 256 bytes or not UTF-8, or the per-call write limit is hit |

A call that uses a capability it did not declare traps that call (`CallError::CapabilityDenied`)
and leaves the host running.

## Limits

Every call runs in a fresh `Store` with a memory cap, an epoch deadline
(`Limits::call_timeout`) and the log and state-write limits above. Modules are capped at
8 MiB. A call returns its `Effects` (state writes, log lines) and applies nothing itself:
the caller commits them as one entry, which is how the controller will tag them with the
leader term (automation hooks spec, "Commit rule").
