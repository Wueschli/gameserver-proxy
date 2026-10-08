# Plugin host foundation (Wave 5, slice 1) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the plugin ABI crate and a standalone plugin host crate that loads a WASM plugin, enforces its approved capabilities and limits, and runs `init` and `on_timer` with the `log` and `state` capabilities. No controller wiring yet.

**Architecture:** `wayhouse-plugin-abi` is the dependency-free guest helper (version constants, section stamp, `alloc`). `wayhouse-plugin-host` is a library that reads the ABI and capability sections without compiling (as `read_abi_version` does for sniffers), compiles under size and limit caps, and runs each call in a fresh `Store` with an epoch deadline and memory cap. A call returns its effects (`state` writes, log lines) as one value for the caller to commit; the host never writes the store itself, which is what lets the later controller slice commit them as one term-checked entry.

**Tech Stack:** Rust, wasmtime 48 (workspace pin), wasmparser, serde/serde_json, `wat` for test guests.

**Spec:** `docs/superpowers/specs/2026-10-07-plugin-system-design.md` (#221) and `docs/superpowers/specs/2026-10-07-plugin-automation-hooks-design.md` (#234; wins where they differ). Phasing in the addendum: `on_timer` + `http` + `state` + `log` first. This slice does `on_timer`, `state`, `log`; `http` needs #235 (secrets) and the network rules and is the next slice.

## Global Constraints

- Custom section `wayhouse.plugin-abi` (major u16 LE, minor u16 LE), exact minor match while major is 0, checked before compile.
- Capabilities are embedded in the custom section `wayhouse.plugin-caps` (JSON), read without instantiating, bound to the module's sha256.
- Module size cap 8 MiB; `tick_interval` minimum 10 s, enforced by the host.
- A call without the capability traps the call, not the host.
- Fresh `Store` per call; memory cap and epoch timeout as for sniffers.
- Guest exports: `memory`, `alloc(i32)->i32`, `init(i32,i32)`, `on_timer()`. Host imports live in the `wayhouse` module namespace.
- Sniffer ABI and crate stay untouched.

## Review Focus

- A guest with a `state_put` value larger than the cap: the call is refused (`-1`), the host does not panic.
- A guest that passes out-of-bounds pointers to an import: the call traps, the host survives.
- A module with two `wayhouse.plugin-caps` sections, malformed JSON or unknown fields: rejected at validation.
- An import outside the published set (for example `wasi_snapshot_preview1::fd_write`): rejected at validation.
- A guest that spins forever in `on_timer`: the epoch deadline ends the call as `Timeout`.

---

### Task 1: `wayhouse-plugin-abi` guest crate

**Files:**
- Create: `crates/wayhouse-plugin-abi/Cargo.toml`, `crates/wayhouse-plugin-abi/src/lib.rs`
- Modify: `Cargo.toml` (workspace members, if listed explicitly)

**Interfaces:**
- Produces: `pub const ABI_MAJOR: u16 = 0`, `pub const ABI_MINOR: u16 = 1`, `pub const ABI_BYTES: [u8; 4]`, `pub const ABI_SECTION: &str = "wayhouse.plugin-abi"`, `pub const CAPS_SECTION: &str = "wayhouse.plugin-caps"`, `#[no_mangle] pub extern "C" fn alloc(len: u32) -> u32`.

- [ ] **Step 1: Test** `abi_bytes_are_major_then_minor_le` asserts `ABI_BYTES == [0,0,1,0]`; `alloc_zero_returns_zero`; `alloc_returns_writable_region`.
- [ ] **Step 2: Run** `cargo test -p wayhouse-plugin-abi` — fails (crate missing).
- [ ] **Step 3: Implement** the crate like `wayhouse-sniffer-abi` (no dependencies, `#[cfg(target_arch = "wasm32")]` section stamp).
- [ ] **Step 4: Run** the tests — pass.
- [ ] **Step 5: Commit** `feat(plugin): add the plugin ABI guest crate`.

### Task 2: Module inspection and capability parsing

**Files:**
- Create: `crates/wayhouse-plugin-host/Cargo.toml`, `src/lib.rs`, `src/module.rs`, `src/caps.rs`

**Interfaces:**
- Produces in `caps.rs`: `pub struct Capabilities { pub triggers: Triggers, pub tick_interval_secs: u64, pub log: bool, pub state: Option<StateCap> }`, `pub struct Triggers { pub on_timer: bool }`, `pub struct StateCap { pub max_bytes: usize }`, `Capabilities::parse(&[u8]) -> Result<Self, ModuleError>`. Serde with `deny_unknown_fields`.
- Produces in `module.rs`: `pub const MAX_MODULE_BYTES: usize = 8 * 1024 * 1024`, `pub struct AbiVersion {major, minor}`, `pub struct ModuleInfo { pub abi: AbiVersion, pub caps: Capabilities, pub sha256: String }`, `pub enum ModuleError` (Empty, TooLarge, AbiMissing, AbiMalformed, AbiIncompatible, CapsMissing, CapsDuplicate, CapsInvalid(String), TickIntervalTooShort, UndeclaredTimer, Compile, UnexpectedImport, MissingExport, WrongSignature, Instantiate), `pub fn inspect(bytes: &[u8]) -> Result<ModuleInfo, ModuleError>`.

- [ ] **Step 1: Tests** in `module.rs` using `wat` fixtures with custom sections: accepts a fixture with a valid abi + caps; `inspect_rejects_empty`, `_oversize`, `_missing_abi`, `_wrong_minor`, `_missing_caps`, `_two_caps_sections`, `_unknown_caps_field`, `_tick_interval_below_10s`; `on_timer` trigger with `tick_interval_secs = 0` rejected.
- [ ] **Step 2: Run** `cargo test -p wayhouse-plugin-host` — fails.
- [ ] **Step 3: Implement** `inspect` with `wasmparser` (custom sections only, nothing compiled), sha256 via `sha2`.
- [ ] **Step 4: Run** — pass.
- [ ] **Step 5: Commit** `feat(plugin): inspect plugin modules and parse embedded capabilities`.

### Task 3: Runtime with `init`, `on_timer`, `log`, `state`

**Files:**
- Create: `crates/wayhouse-plugin-host/src/runtime.rs`, `tests/runtime.rs`

**Interfaces:**
- Consumes: Task 2's `inspect`, `ModuleInfo`, `Capabilities`.
- Produces: `pub struct PluginHost` (`PluginHost::new(limits: Limits) -> anyhow::Result<Self>` builds the engine and one epoch-ticker thread), `pub struct Limits { call_timeout: Duration, max_memory_bytes: usize, max_table_elements: usize, max_log_lines: usize, max_state_ops: usize }` with `Default`, `PluginHost::load(&self, bytes: &[u8], approved: &Capabilities) -> Result<Plugin, ModuleError>` (inspect, compile, import and export checks, trial instantiation), `Plugin::info(&self) -> &ModuleInfo`, `Plugin::init(&self, config: &[u8], state: &StateSnapshot) -> Result<Effects, CallError>`, `Plugin::on_timer(&self, state: &StateSnapshot) -> Result<Effects, CallError>`, `pub type StateSnapshot = BTreeMap<String, Vec<u8>>`, `pub struct Effects { pub state_puts: BTreeMap<String, Vec<u8>>, pub logs: Vec<LogLine> }`, `pub struct LogLine { pub level: LogLevel, pub msg: String }`, `pub enum CallError { Timeout, Trap(String), CapabilityDenied(&'static str) }`.
- Imports (module `wayhouse`): `log(level: i32, ptr: i32, len: i32)`; `state_get(kptr, klen, out_ptr, out_cap) -> i32` (value length, `-1` absent, nothing written when it exceeds `out_cap`); `state_put(kptr, klen, vptr, vlen) -> i32` (`0` ok, `-1` over the cap). `state_get` sees the call's own pending puts first.

- [ ] **Step 1: Tests** (WAT guests): `on_timer_logs_and_writes_state`; `init_receives_config`; `state_get_sees_snapshot_and_own_writes`; `state_put_over_cap_returns_minus_one`; `state_without_capability_traps_with_capability_denied`; `log_without_capability_traps`; `out_of_bounds_pointer_traps_the_call_only`; `endless_loop_times_out`; `memory_growth_past_cap_traps`; `log_lines_beyond_the_limit_are_dropped`; `import_outside_the_set_is_rejected_at_load`; `missing_on_timer_export_is_rejected_when_declared`.
- [ ] **Step 2: Run** — fails.
- [ ] **Step 3: Implement** with a `Linker<CallState>`; effects accumulate in `CallState` and are returned, never applied. Classify `Trap::Interrupt` as `Timeout`.
- [ ] **Step 4: Run** `cargo test -p wayhouse-plugin-host` — pass.
- [ ] **Step 5: Commit** `feat(plugin): run plugin init and on_timer with log and state capabilities`.

### Task 4: Docs and bookkeeping

**Files:**
- Modify: `AGENTS.md` (layout table and touch-table rows), `HANDOVER.md` (Resume here), `docs/superpowers/plans/2026-10-05-transition-index.md` (Wave 5 row)
- Create: `docs/plugins.md` (ABI, exports, imports, limits as built)

- [ ] **Step 1: Write** the docs; list the follow-up slices (controller host and API, secrets #235, `http`, routes #236, webhooks #237, UI, registry `kind`).
- [ ] **Step 2: Run** `make check` — green.
- [ ] **Step 3: Commit** `docs: describe the plugin host foundation`.
