# Plugin compile bounds and conformance check (Wave 5, slice 2) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bound what compiling a hostile plugin module can cost, run compilation on a bounded pool with a timeout, and ship a conformance check plugin authors and CI can run against a built `.wasm`.

**Architecture:** A `bounds` pre-scan walks the module with `wasmparser` before `Module::new` and rejects structures that make compilation expensive. `CompilePool` is a fixed set of worker threads behind a bounded queue; a caller waits with a timeout and gets `Busy` when the queue is full. A `conformance` module and the `wayhouse-plugin-check` binary load a module with its own declaration as the approved set, run the entry points, and report what the plugin used.

**Tech Stack:** Rust, wasmparser, wasmtime 48, std threads and channels.

**Spec:** `docs/superpowers/specs/2026-10-07-plugin-system-design.md` ("Plugin ABI", Risks) and the addendum. Issues: #265, #266. Builds on [slice 1](2026-10-08-plugin-host-foundation.md).

## Global Constraints

- Module size cap stays 8 MiB; bounds are checked before `Module::new`.
- Compilation never runs on the caller's thread when it goes through `CompilePool`.
- A rejected module reports which bound it hit.
- Declared `state.max_bytes` has a host ceiling of 1 MiB.
- `Limits::call_timeout` of zero is rejected by `PluginHost::new`.

## Review Focus

- A module with 100 000 functions, 100 000 locals in one function, or 100 000 nested blocks is rejected without compiling.
- A full compile queue answers `Busy` at once, not after the timeout.
- A compile that outlasts the timeout returns `TimedOut` and the pool stays usable.
- The check tool exits non-zero for a module the host would reject and prints the reason.
- A plugin that calls `log` without declaring it is reported as a capability violation, not a crash.

---

### Task 1: `bounds` pre-scan

**Files:** Create `crates/wayhouse-plugin-host/src/bounds.rs`; modify `module.rs` (`ModuleError::TooComplex(String)`), `runtime.rs` (`load` calls the scan), `lib.rs`.

**Interfaces:**
- Produces: `pub struct Bounds { max_functions, max_types, max_locals_per_function, max_nesting_depth, max_table_elements_declared, max_memory_pages_declared, max_globals, max_imports_exports, max_code_bytes }` with `Default`; `pub fn check(bytes: &[u8], bounds: &Bounds) -> Result<(), ModuleError>`; `Limits.bounds: Bounds`.

- [ ] **Step 1:** tests in `tests/bounds.rs` with generated WAT: 100 000 empty functions, one function with 100 000 locals, 100 000 nested `block`s, a table of 100 000 000 elements, a memory declared at 65 536 pages, 10 000 globals, each rejected with `TooComplex` naming the bound; a normal module passes.
- [ ] **Step 2:** run, expect FAIL.
- [ ] **Step 3:** implement `check` with a `wasmparser::Parser` walk; nesting depth from `OperatorsReader` (`block`/`loop`/`if` push, `end` pop). Wire into `PluginHost::load` after `inspect`.
- [ ] **Step 4:** run, expect PASS. **Step 5:** commit `feat(plugin): bound module structure before compiling`.

### Task 2: `CompilePool`

**Files:** Create `src/pool.rs`; modify `lib.rs`.

**Interfaces:**
- Consumes: `PluginHost::load(&self, &[u8], &Capabilities)`.
- Produces: `pub struct CompilePool`, `CompilePool::new(host: Arc<PluginHost>, workers: usize, queue: usize, timeout: Duration) -> anyhow::Result<Self>`, `CompilePool::load(&self, bytes: Vec<u8>, approved: Capabilities) -> Result<Plugin, PoolError>`, `pub enum PoolError { Module(ModuleError), Busy, TimedOut }`.

- [ ] **Step 1:** tests: loads a valid module on a worker thread (assert thread name differs from the caller's); `Busy` when workers and queue are full (hold workers with a gate via a test hook); `TimedOut` for a zero-length timeout while the pool stays usable afterwards.
- [ ] **Step 2:** run, FAIL. **Step 3:** implement with `std::sync::mpsc::sync_channel(queue)`, workers sharing the receiver behind a `Mutex`, per-job reply channel and `recv_timeout`.
- [ ] **Step 4:** PASS. **Step 5:** commit `feat(plugin): compile modules on a bounded pool with a timeout`.

### Task 3: Host fixes from the slice 1 review

**Files:** `caps.rs`, `runtime.rs`, tests.

- [ ] **Step 1:** tests: `state.max_bytes` over 1 MiB rejected by `Capabilities::parse` (`CapsInvalid`); `PluginHost::new` with `call_timeout` zero errors; `Effects.used_log` / `used_state` set when the guest calls the import.
- [ ] **Step 2:** FAIL. **Step 3:** implement (`MAX_STATE_BYTES`, `Limits` validation, `used_*` flags on `Effects`).
- [ ] **Step 4:** PASS. **Step 5:** commit `fix(plugin): host ceiling on state size, reject zero timeout, report used capabilities`.

### Task 4: Conformance check

**Files:** Create `src/conformance.rs`, `src/bin/wayhouse-plugin-check.rs`; modify `Cargo.toml` (`[[bin]]`), `docs/plugins.md`, `AGENTS.md`, `HANDOVER.md`.

**Interfaces:**
- Produces: `pub fn check(bytes: &[u8]) -> Report`; `Report { info: Option<ModuleInfo>, problems: Vec<String>, init: Option<Effects>, on_timer: Option<Effects> }`, `Report::ok()`.

- [ ] **Step 1:** tests: a good module passes and lists its used capabilities; a module with a bad ABI section fails with the reason; a guest that traps in `on_timer` is a problem, not a panic; a guest using `log` without declaring it is reported as a capability violation.
- [ ] **Step 2:** FAIL. **Step 3:** implement; the binary reads a path, prints the report, exits 1 when `!ok()`.
- [ ] **Step 4:** PASS, then `make check`. **Step 5:** commit `feat(plugin): wayhouse-plugin-check conformance tool`.
