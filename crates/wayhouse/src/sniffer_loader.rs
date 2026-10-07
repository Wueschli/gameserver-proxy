//! Phase 9 slice 3: the WASM sniffer plugin loader.
//!
//! Loads `*.wasm` modules from `settings.sniffers.dir` into a
//! [`wayhouse_core::sniff::Sniffers`] registry. Each module is a **core WASM
//! module — no WASI, no host imports**: a plugin cannot touch the filesystem,
//! the clock, or the network; it only ever sees the bytes it's handed and
//! returns a result. `wasmtime` is a binary-only dependency (like `reqwest`)
//! — `wayhouse-core` has no sandboxing dependency, only the `Sniffer` trait /
//! `Sniffers` registry it calls into.
//!
//! ## ABI
//! The guest exports `memory`, `alloc(len: i32) -> i32` (returns a pointer to
//! `len` free, host-writable bytes) and
//! `sniff(in_ptr: i32, in_len: i32, cfg_ptr: i32, cfg_len: i32) -> i64`. The
//! host `alloc`s + writes two regions before the call: the peeked first bytes
//! (`in_*`) and this module's `settings.sniffers.modules[].config` string bytes
//! (`cfg_*`; `cfg_len == 0` and `cfg_ptr` meaningless when the module has no
//! configured `config`). The return value: `0` means "not recognised"; any
//! other value is the packed `(result_ptr << 32) | result_len`, pointing at a
//! compact encoding of a `RouteHint`:
//!
//! ```text
//! byte 0:            flags (bit0 = reject, bit1 = host present, bit2 = key present)
//! if host present:    u16 LE length, then that many UTF-8 bytes
//! if key present:     u16 LE length, then that many UTF-8 bytes
//! ```
//!
//! This encoding is also what the `wayhouse-sniffer-abi` guest helper crate
//! (`crates/plugins/`) implements on the write side.
//!
//! ### Version
//! A module declares the ABI version it was built for in a custom section named
//! `wayhouse.abi`: exactly 4 bytes, major then minor, both `u16` LE (now
//! `0.1`, [`HOST_ABI`]). Depending on `wayhouse-sniffer-abi` adds the section to
//! a plugin; nothing else to do. The host reads it with [`read_abi_version`]
//! before compiling, so no guest code runs, and rejects a module without the
//! section, with a duplicate or wrong-sized one, or with another version. While
//! the major is 0 the minor must match exactly (a 0.x minor bump may break the
//! ABI); at 1.0 this relaxes to "same major, plugin minor <= host minor" by a
//! deliberate decision. Other custom sections are ignored.
//!
//! ## Bounds
//! Two independent bounds keep a plugin from stalling or ballooning the
//! process: a shared `wasmtime::Engine` with epoch interruption — a ticker
//! thread bumps the engine's epoch every `call_timeout_ms / 2`, and every call
//! sets a two-tick deadline. The first tick can land anywhere after the call
//! starts, so a call is guaranteed at least half of `call_timeout_ms` and is
//! trapped by `call_timeout_ms` at the latest (a ceiling, not an exact limit)
//! — and a per-call `StoreLimits` memory cap (`max_memory_bytes`). Every
//! call gets a fresh `Store` + `Instance`; no state survives between
//! connections (deliberate: simplicity over instance-reuse latency, revisited
//! in slice 6 if the per-call instantiate cost misses NFR N1).

use std::collections::HashMap;
use std::fs;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use wasmtime::{
    Config, Engine, ExternType, FuncType, Instance, Module, Store, StoreLimits, StoreLimitsBuilder,
    ValType,
};

use wayhouse_config::{RouteHint, SnifferModulePin, SniffersConfig};
use wayhouse_core::metrics_defs as m;
use wayhouse_core::sniff::{Sniffer, Sniffers};

struct StoreState {
    limits: StoreLimits,
}

/// One loaded plugin module, ready to be instantiated per call.
pub struct WasmSniffer {
    name: String,
    engine: Arc<Engine>,
    module: Module,
    max_memory_bytes: usize,
    /// This module's `settings.sniffers.modules[].config` string as bytes
    /// (empty when unset); marshalled into linear memory on every `sniff` call.
    config: Vec<u8>,
}

/// Epoch ticks a call may span before it traps. A deadline of one tick fires
/// at the *next* tick, which may be arbitrarily soon after the call starts, so
/// the ticker runs at `call_timeout / EPOCH_DEADLINE_TICKS` and a call gets
/// between `call_timeout / 2` and `call_timeout`.
const EPOCH_DEADLINE_TICKS: u64 = 2;

enum CallError {
    /// The epoch deadline fired before the call returned.
    Timeout,
    /// Any other WASM trap, or an instantiation / lookup failure.
    Trap,
    /// The plugin returned a result the host couldn't decode.
    BadOutput,
}

impl Sniffer for WasmSniffer {
    fn name(&self) -> &str {
        &self.name
    }

    fn sniff(&self, first: &[u8]) -> Option<RouteHint> {
        let start = std::time::Instant::now();
        let result = self.call(first);
        metrics::histogram!(m::SNIFFER_CALL_SECONDS, "name" => self.name.clone())
            .record(start.elapsed().as_secs_f64());

        let (label, hint) = match result {
            Ok(hint) => ("ok", hint),
            Err(CallError::Timeout) => ("timeout", None),
            Err(CallError::Trap) => ("trap", None),
            Err(CallError::BadOutput) => ("bad_output", None),
        };
        // A clean "not recognised" (packed == 0) also lands as Ok(None) above
        // but must not count as an error result.
        let label = if label == "ok" && hint.is_none() {
            "unrecognised"
        } else {
            label
        };
        metrics::counter!(m::SNIFFER_CALLS, "name" => self.name.clone(), "result" => label)
            .increment(1);
        hint
    }
}

impl WasmSniffer {
    fn call(&self, first: &[u8]) -> Result<Option<RouteHint>, CallError> {
        let mut store = new_store(&self.engine, self.max_memory_bytes);

        let instance = Instance::new(&mut store, &self.module, &[]).map_err(classify)?;
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or(CallError::Trap)?;
        let alloc = instance
            .get_typed_func::<i32, i32>(&mut store, "alloc")
            .map_err(|_| CallError::Trap)?;
        let sniff = instance
            .get_typed_func::<(i32, i32, i32, i32), i64>(&mut store, "sniff")
            .map_err(|_| CallError::Trap)?;

        // `alloc` + copy a region into guest memory; a zero-length region is
        // passed as `(0, 0)` and never allocated (matches `wayhouse_sniffer_abi::alloc`).
        let write_region =
            |store: &mut Store<StoreState>, bytes: &[u8]| -> Result<(i32, i32), CallError> {
                if bytes.is_empty() {
                    return Ok((0, 0));
                }
                let len = i32::try_from(bytes.len()).map_err(|_| CallError::BadOutput)?;
                let ptr = alloc.call(&mut *store, len).map_err(classify)?;
                memory
                    .write(&mut *store, ptr as usize, bytes)
                    .map_err(|_| CallError::BadOutput)?;
                Ok((ptr, len))
            };
        let (in_ptr, in_len) = write_region(&mut store, first)?;
        let (cfg_ptr, cfg_len) = write_region(&mut store, &self.config)?;

        let packed = sniff
            .call(&mut store, (in_ptr, in_len, cfg_ptr, cfg_len))
            .map_err(classify)?;
        if packed == 0 {
            return Ok(None);
        }
        let out_ptr = ((packed as u64) >> 32) as usize;
        let out_len = ((packed as u64) & 0xffff_ffff) as usize;
        let bytes = memory
            .data(&store)
            .get(out_ptr..out_ptr.wrapping_add(out_len))
            .ok_or(CallError::BadOutput)?;
        decode_route_hint(bytes)
            .map(Some)
            .ok_or(CallError::BadOutput)
    }
}

/// A fresh per-call `Store` with the memory cap and epoch deadline applied.
/// Shared by `sniff` calls and by [`SnifferLoader::validate`] so the limits a
/// module is vetted under cannot drift from the ones it runs under.
fn new_store(engine: &Engine, max_memory_bytes: usize) -> Store<StoreState> {
    let state = StoreState {
        limits: StoreLimitsBuilder::new()
            .memory_size(max_memory_bytes)
            .build(),
    };
    let mut store = Store::new(engine, state);
    store.limiter(|s| &mut s.limits);
    store.set_epoch_deadline(EPOCH_DEADLINE_TICKS);
    store
}

/// Largest module the loader and the admin upload accept.
pub const MAX_MODULE_BYTES: usize = 8 * 1024 * 1024;

/// The ABI version a sniffer module declares (or the host speaks).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AbiVersion {
    pub major: u16,
    pub minor: u16,
}

impl std::fmt::Display for AbiVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// The ABI version this host implements.
pub const HOST_ABI: AbiVersion = AbiVersion { major: 0, minor: 1 };

/// Name of the custom section a module declares its ABI version in.
const ABI_SECTION: &str = "wayhouse.abi";

impl AbiVersion {
    /// While the major is 0 a minor bump may break the ABI (SemVer 0.x), so the
    /// versions must match exactly. At 1.0 this relaxes to "same major, plugin
    /// minor <= host minor" — a policy decision for then, not now.
    fn accepted_by(self, host: Self) -> bool {
        self.major == host.major && self.minor == host.minor
    }
}

/// Read the ABI version out of the module's `wayhouse.abi` custom section
/// (4 bytes: major u16 LE, minor u16 LE). The module is only parsed, never
/// compiled or instantiated, so no guest code runs. Other custom sections are
/// ignored; a second `wayhouse.abi` section or a payload of any other length is
/// rejected.
pub fn read_abi_version(bytes: &[u8]) -> Result<AbiVersion, ModuleError> {
    let mut found: Option<&[u8]> = None;
    for payload in wasmparser::Parser::new(0).parse_all(bytes) {
        let payload = payload.map_err(|e| ModuleError::Compile(e.to_string()))?;
        if let wasmparser::Payload::CustomSection(c) = payload {
            if c.name() == ABI_SECTION {
                if found.is_some() {
                    return Err(ModuleError::AbiMalformed(c.data().len()));
                }
                found = Some(c.data());
            }
        }
    }
    match found {
        None => Err(ModuleError::AbiMissing),
        Some(&[a, b, c, d]) => Ok(AbiVersion {
            major: u16::from_le_bytes([a, b]),
            minor: u16::from_le_bytes([c, d]),
        }),
        Some(other) => Err(ModuleError::AbiMalformed(other.len())),
    }
}

/// Why a byte string is not a loadable sniffer module.
#[derive(Debug)]
pub enum ModuleError {
    AbiMissing,
    AbiMalformed(usize),
    AbiIncompatible {
        plugin: AbiVersion,
        host: AbiVersion,
    },
    TooLarge {
        len: usize,
        max: usize,
    },
    Empty,
    Compile(String),
    UnexpectedImport(String),
    MissingExport(&'static str),
    WrongSignature(&'static str),
    Instantiate(String),
}

impl std::fmt::Display for ModuleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge { len, max } => write!(f, "module is {len} bytes, the limit is {max}"),
            Self::Empty => write!(f, "module is empty"),
            Self::AbiMissing => write!(
                f,
                "module declares no ABI version (custom section `{ABI_SECTION}`); build it \
                 against `wayhouse-sniffer-abi` {HOST_ABI}, which adds the section"
            ),
            Self::AbiMalformed(len) => write!(
                f,
                "custom section `{ABI_SECTION}` must be one 4-byte section, found a {len}-byte payload \
                 or a duplicate"
            ),
            Self::AbiIncompatible { plugin, host } => write!(
                f,
                "module targets sniffer ABI {plugin}, this host speaks {host}"
            ),
            Self::Compile(e) => write!(f, "not a valid wasm module: {e}"),
            Self::UnexpectedImport(i) => {
                write!(f, "module imports {i}, but sniffer modules take no imports")
            }
            Self::MissingExport(n) => write!(f, "module does not export `{n}`"),
            Self::WrongSignature(n) => write!(f, "export `{n}` has the wrong type"),
            Self::Instantiate(e) => write!(f, "module does not instantiate: {e}"),
        }
    }
}

impl std::error::Error for ModuleError {}

#[allow(clippy::needless_pass_by_value)] // `map_err` callback, which hands the error over by value
fn classify(e: wasmtime::Error) -> CallError {
    if let Some(trap) = e.downcast_ref::<wasmtime::Trap>() {
        if *trap == wasmtime::Trap::Interrupt {
            return CallError::Timeout;
        }
    }
    CallError::Trap
}

/// Decode the compact `RouteHint` encoding described in the module doc.
/// `None` on any truncation / bad-UTF-8 / trailing-garbage shape — the caller
/// counts that as `bad_output` rather than trusting a malformed plugin result.
fn decode_route_hint(bytes: &[u8]) -> Option<RouteHint> {
    fn read_string(bytes: &[u8], pos: &mut usize) -> Option<String> {
        let len = u16::from_le_bytes(bytes.get(*pos..*pos + 2)?.try_into().ok()?) as usize;
        *pos += 2;
        let s = bytes.get(*pos..*pos + len)?;
        *pos += len;
        std::str::from_utf8(s).ok().map(str::to_string)
    }

    let mut pos = 0usize;
    let flags = *bytes.first()?;
    pos += 1;
    let reject = flags & 0b001 != 0;
    let has_host = flags & 0b010 != 0;
    let has_key = flags & 0b100 != 0;

    let host = if has_host {
        Some(read_string(bytes, &mut pos)?)
    } else {
        None
    };
    let key = if has_key {
        Some(read_string(bytes, &mut pos)?)
    } else {
        None
    };

    Some(RouteHint { host, key, reject })
}

/// A persistent handle to the shared `wasmtime::Engine` and its epoch-ticker
/// thread — built once at startup and reused for every rescan of
/// `settings.sniffers.dir` on a config reload (phase 9 slice 4). `dir` (and
/// `modules` pins) can differ between calls to [`SnifferLoader::scan`]; the
/// engine itself — and therefore `call_timeout_ms` — is fixed for the life of
/// the process, like `settings.workers`.
pub struct SnifferLoader {
    engine: Arc<Engine>,
    /// `settings.sniffers.modules` as of the latest [`SnifferLoader::scan`],
    /// so the admin upload can refuse what the next scan would reject.
    pins: std::sync::Mutex<Vec<SnifferModulePin>>,
}

impl SnifferLoader {
    /// Build the engine and spawn its epoch-ticker thread (bumps the epoch
    /// every `call_timeout / 2`, forever — one thread for the process, not one
    /// per plugin or per call).
    pub fn new(call_timeout: std::time::Duration) -> Result<Self> {
        let tick = call_timeout / EPOCH_DEADLINE_TICKS as u32;
        let mut engine_cfg = Config::new();
        engine_cfg.epoch_interruption(true);
        let engine = Arc::new(
            Engine::new(&engine_cfg)
                .map_err(|e| anyhow::anyhow!("building the sniffer wasm engine: {e}"))?,
        );
        {
            let engine = engine.clone();
            std::thread::Builder::new()
                .name("wayhouse-sniffer-epoch".into())
                .spawn(move || loop {
                    std::thread::sleep(tick);
                    engine.increment_epoch();
                })
                .context("spawning the sniffer epoch-ticker thread")?;
        }
        Ok(Self {
            engine,
            pins: std::sync::Mutex::new(Vec::new()),
        })
    }

    /// The module pins of the latest scan (empty: no pinning).
    pub fn pins(&self) -> Vec<SnifferModulePin> {
        self.pins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Check that `bytes` is a module this loader would accept: within
    /// [`MAX_MODULE_BYTES`], a compatible ABI version declaration, no imports, the three ABI exports with the right
    /// types, and it instantiates under `max_memory_bytes`. `sniff` is not called.
    pub fn validate(&self, bytes: &[u8], max_memory_bytes: usize) -> Result<(), ModuleError> {
        self.compile_checked(bytes, max_memory_bytes).map(drop)
    }

    fn compile_checked(
        &self,
        bytes: &[u8],
        max_memory_bytes: usize,
    ) -> Result<Module, ModuleError> {
        if bytes.is_empty() {
            return Err(ModuleError::Empty);
        }
        if bytes.len() > MAX_MODULE_BYTES {
            return Err(ModuleError::TooLarge {
                len: bytes.len(),
                max: MAX_MODULE_BYTES,
            });
        }
        let plugin = read_abi_version(bytes)?;
        if !plugin.accepted_by(HOST_ABI) {
            return Err(ModuleError::AbiIncompatible {
                plugin,
                host: HOST_ABI,
            });
        }
        let module =
            Module::new(&self.engine, bytes).map_err(|e| ModuleError::Compile(e.to_string()))?;
        if let Some(i) = module.imports().next() {
            return Err(ModuleError::UnexpectedImport(format!(
                "{}::{}",
                i.module(),
                i.name()
            )));
        }
        let want = |name: &'static str, params: &[ValType], results: &[ValType]| match module
            .get_export(name)
        {
            Some(ExternType::Func(ft)) => {
                let expected = FuncType::new(
                    &self.engine,
                    params.iter().cloned(),
                    results.iter().cloned(),
                );
                if FuncType::eq(&ft, &expected) {
                    Ok(())
                } else {
                    Err(ModuleError::WrongSignature(name))
                }
            }
            Some(_) => Err(ModuleError::WrongSignature(name)),
            None => Err(ModuleError::MissingExport(name)),
        };
        match module.get_export("memory") {
            Some(ExternType::Memory(_)) => {}
            Some(_) => return Err(ModuleError::WrongSignature("memory")),
            None => return Err(ModuleError::MissingExport("memory")),
        }
        want("alloc", &[ValType::I32], &[ValType::I32])?;
        want(
            "sniff",
            &[ValType::I32, ValType::I32, ValType::I32, ValType::I32],
            &[ValType::I64],
        )?;

        let mut store = new_store(&self.engine, max_memory_bytes);
        Instance::new(&mut store, &module, &[])
            .map_err(|e| ModuleError::Instantiate(e.to_string()))?;
        Ok(module)
    }

    /// Scan `cfg.dir` for `*.wasm` modules and compile each into a
    /// [`WasmSniffer`], returning the resulting name→plugin map. A module is
    /// named after its file stem (`a2s.wasm` → sniffer `a2s`). When
    /// `cfg.modules` is non-empty every loaded file must have a matching pin
    /// (`name` + `sha256`) — an unpinned or hash-mismatched file fails the
    /// whole scan, and so does a pinned module that is not loadable (see
    /// [`SnifferLoader::validate`]; a silent skip would downgrade the pin), so
    /// the caller keeps its old registry rather than a half-updated one. When
    /// nothing is pinned, a file that is not a loadable module is logged and
    /// skipped so one bad `.wasm` cannot block rescans. A module's `modules[].config`
    /// string, if any, is baked onto its `WasmSniffer` here and handed to the
    /// guest on every `sniff` call.
    pub fn scan(&self, cfg: &SniffersConfig) -> Result<HashMap<String, Arc<dyn Sniffer>>> {
        *self
            .pins
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = cfg.modules.clone();
        let mut modules = HashMap::new();
        let entries = fs::read_dir(&cfg.dir)
            .with_context(|| format!("settings.sniffers.dir {:?}", cfg.dir))?;
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("wasm") {
                continue;
            }
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();
            let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;

            let pin = cfg.modules.iter().find(|m| m.name == name);
            if !cfg.modules.is_empty() {
                let digest = format!("{:x}", Sha256::digest(&bytes));
                match pin {
                    Some(p) if p.sha256 == digest => {}
                    Some(p) => bail!(
                        "sniffer {name}: sha256 mismatch (pinned {}, loaded {digest})",
                        p.sha256
                    ),
                    None => bail!("sniffer {name}: not listed in settings.sniffers.modules"),
                }
            }
            let config = pin
                .and_then(|p| p.config.clone())
                .unwrap_or_default()
                .into_bytes();

            let module = match self.compile_checked(&bytes, cfg.max_memory_bytes) {
                Ok(m) => m,
                Err(e) if pin.is_some() => {
                    bail!("sniffer {name}: pinned module failed validation: {e}")
                }
                Err(e) => {
                    tracing::error!(sniffer = %name, error = %e, "sniffer module rejected, skipping");
                    continue;
                }
            };
            let sniffer: Arc<dyn Sniffer> = Arc::new(WasmSniffer {
                name: name.clone(),
                engine: self.engine.clone(),
                module,
                max_memory_bytes: cfg.max_memory_bytes,
                config,
            });
            tracing::info!(sniffer = %name, path = %path.display(), "sniffer plugin loaded");
            modules.insert(name, sniffer);
        }
        Ok(modules)
    }
}

/// Startup convenience: build a [`SnifferLoader`] (and its engine / ticker
/// thread) and do the first [`SnifferLoader::scan`] in one call. The returned
/// loader is kept by the caller (`main.rs`) and handed to the reload task so
/// later scans reuse the same engine instead of leaking a ticker thread per
/// reload.
pub fn build_sniffers(cfg: &SniffersConfig) -> Result<(SnifferLoader, Sniffers)> {
    let loader = SnifferLoader::new(cfg.call_timeout)?;
    let modules = loader.scan(cfg)?;
    let registry = Sniffers::new();
    for sniffer in modules.into_values() {
        registry.register(sniffer);
    }
    Ok((loader, registry))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn cfg(dir: &std::path::Path) -> SniffersConfig {
        SniffersConfig {
            dir: dir.to_string_lossy().into_owned(),
            call_timeout: Duration::from_millis(50),
            max_memory_bytes: 1 << 20,
            modules: Vec::new(),
        }
    }

    /// A minimal plugin: recognises `b"HOST:<name>\n"`, same contract as
    /// `wayhouse_core::sniff::tests::TestHost`, hand-written in WAT so the test
    /// needs no `wasm32-unknown-unknown` toolchain. It walks the input byte
    /// by byte looking for a `\n`, treats everything after `HOST:` (5 bytes)
    /// as the host, and encodes a `RouteHint { host: Some(..), .. }` at a
    /// fixed output offset.
    const HOST_SNIFFER_WAT: &str = r#"
        (module
          (@custom "wayhouse.abi" "\00\00\01\00")
          (memory (export "memory") 2)
          ;; Bump allocator: next free offset lives at address 0 (reserved).
          (global $next (mut i32) (i32.const 4))
          (func $alloc (export "alloc") (param $len i32) (result i32)
            (local $p i32)
            (local.set $p (global.get $next))
            (global.set $next (i32.add (local.get $p) (local.get $len)))
            (local.get $p))

          ;; sniff(in_ptr, in_len, cfg_ptr, cfg_len) -> packed
          ;; (out_ptr << 32 | out_len), 0 if no "HOST:" prefix (first 5 bytes).
          ;; This fixture ignores the cfg_* params.
          (func $sniff (export "sniff")
            (param $ptr i32) (param $len i32) (param $cfg_ptr i32) (param $cfg_len i32)
            (result i64)
            (local $out i32)
            (local $hostlen i32)
            (local $i i32)
            (if (i32.lt_u (local.get $len) (i32.const 5))
              (then (return (i64.const 0))))
            ;; require the literal "HOST:" (0x48 0x4F 0x53 0x54 0x3A)
            (if (i32.ne (i32.load8_u (local.get $ptr)) (i32.const 0x48)) (then (return (i64.const 0))))
            (if (i32.ne (i32.load8_u (i32.add (local.get $ptr) (i32.const 1))) (i32.const 0x4F)) (then (return (i64.const 0))))
            (if (i32.ne (i32.load8_u (i32.add (local.get $ptr) (i32.const 2))) (i32.const 0x53)) (then (return (i64.const 0))))
            (if (i32.ne (i32.load8_u (i32.add (local.get $ptr) (i32.const 3))) (i32.const 0x54)) (then (return (i64.const 0))))
            (if (i32.ne (i32.load8_u (i32.add (local.get $ptr) (i32.const 4))) (i32.const 0x3A)) (then (return (i64.const 0))))
            ;; host runs from ptr+5 to the end of the input (test fixture: no
            ;; trailing bytes after the host in what we feed it).
            (local.set $hostlen (i32.sub (local.get $len) (i32.const 5)))
            ;; reserve output at a fixed high offset so it never collides with
            ;; the bump allocator's input copy.
            (local.set $out (i32.const 65536))
            ;; flags byte: bit1 (host present) set
            (i32.store8 (local.get $out) (i32.const 0x02))
            ;; u16 LE length
            (i32.store8 (i32.add (local.get $out) (i32.const 1)) (local.get $hostlen))
            (i32.store8 (i32.add (local.get $out) (i32.const 2)) (i32.const 0))
            ;; copy the host bytes
            (local.set $i (i32.const 0))
            (block $done
              (loop $copy
                (br_if $done (i32.ge_u (local.get $i) (local.get $hostlen)))
                (i32.store8
                  (i32.add (i32.add (local.get $out) (i32.const 3)) (local.get $i))
                  (i32.load8_u (i32.add (i32.add (local.get $ptr) (i32.const 5)) (local.get $i))))
                (local.set $i (i32.add (local.get $i) (i32.const 1)))
                (br $copy)))
            ;; pack (out << 32) | (3 + hostlen)
            (i64.or
              (i64.shl (i64.extend_i32_u (local.get $out)) (i64.const 32))
              (i64.extend_i32_u (i32.add (i32.const 3) (local.get $hostlen))))))
    "#;

    /// A fixture that ignores the input and emits `RouteHint { host: <config
    /// bytes> }` — proves the host marshals `modules[].config` into the
    /// `cfg_*` region on every call. Returns `0` (not recognised) when the
    /// config is empty (`cfg_len == 0`). Assumes a config shorter than 256 B.
    const CFG_ECHO_WAT: &str = r#"
        (module
          (@custom "wayhouse.abi" "\00\00\01\00")
          (memory (export "memory") 2)
          (global $next (mut i32) (i32.const 4))
          (func $alloc (export "alloc") (param $len i32) (result i32)
            (local $p i32)
            (local.set $p (global.get $next))
            (global.set $next (i32.add (local.get $p) (local.get $len)))
            (local.get $p))
          (func $sniff (export "sniff")
            (param $ptr i32) (param $len i32) (param $cfg_ptr i32) (param $cfg_len i32)
            (result i64)
            (local $out i32)
            (local $i i32)
            (if (i32.eqz (local.get $cfg_len)) (then (return (i64.const 0))))
            (local.set $out (i32.const 65536))
            (i32.store8 (local.get $out) (i32.const 0x02)) ;; flags: host present
            (i32.store8 (i32.add (local.get $out) (i32.const 1)) (local.get $cfg_len))
            (i32.store8 (i32.add (local.get $out) (i32.const 2)) (i32.const 0))
            (local.set $i (i32.const 0))
            (block $done
              (loop $copy
                (br_if $done (i32.ge_u (local.get $i) (local.get $cfg_len)))
                (i32.store8
                  (i32.add (i32.add (local.get $out) (i32.const 3)) (local.get $i))
                  (i32.load8_u (i32.add (local.get $cfg_ptr) (local.get $i))))
                (local.set $i (i32.add (local.get $i) (i32.const 1)))
                (br $copy)))
            (i64.or
              (i64.shl (i64.extend_i32_u (local.get $out)) (i64.const 32))
              (i64.extend_i32_u (i32.add (i32.const 3) (local.get $cfg_len))))))
    "#;

    fn wasm_sniffer(name: &str, wat: &str, engine: &Arc<Engine>) -> WasmSniffer {
        wasm_sniffer_cfg(name, wat, engine, Vec::new())
    }

    fn wasm_sniffer_cfg(
        name: &str,
        wat: &str,
        engine: &Arc<Engine>,
        config: Vec<u8>,
    ) -> WasmSniffer {
        let bytes = wat::parse_str(wat).unwrap();
        let module = Module::new(engine, &bytes).unwrap();
        WasmSniffer {
            name: name.to_string(),
            engine: engine.clone(),
            module,
            max_memory_bytes: 1 << 20,
            config,
        }
    }

    fn epoch_engine() -> Arc<Engine> {
        let mut c = Config::new();
        c.epoch_interruption(true);
        Arc::new(Engine::new(&c).unwrap())
    }

    #[test]
    fn decode_route_hint_round_trips_host_and_key() {
        // flags = host + key, "a" then "bc"
        let bytes = [0b110u8, 1, 0, b'a', 2, 0, b'b', b'c'];
        let hint = decode_route_hint(&bytes).unwrap();
        assert_eq!(hint.host.as_deref(), Some("a"));
        assert_eq!(hint.key.as_deref(), Some("bc"));
        assert!(!hint.reject);
    }

    #[test]
    fn decode_route_hint_rejects_truncated_or_bad_utf8() {
        assert!(decode_route_hint(&[]).is_none());
        assert!(decode_route_hint(&[0b010, 5, 0, b'a']).is_none()); // len says 5, only 1 byte
        assert!(decode_route_hint(&[0b010, 1, 0, 0xff]).is_none()); // invalid utf8
    }

    #[test]
    fn wasm_plugin_recognises_the_host_end_to_end() {
        let engine = epoch_engine();
        let sniffer = wasm_sniffer("test-host", HOST_SNIFFER_WAT, &engine);
        let hint = sniffer.sniff(b"HOST:survival.example.net").unwrap();
        assert_eq!(hint.host.as_deref(), Some("survival.example.net"));
        assert!(sniffer.sniff(b"nope").is_none());
    }

    #[test]
    fn wasm_plugin_call_times_out_under_the_epoch_deadline() {
        // An infinite loop, no memory/sniff exports needed beyond what the
        // host looks up before calling — the trap must fire during the call.
        let wat = r#"
            (module
              (memory (export "memory") 1)
              (func $alloc (export "alloc") (param i32) (result i32) (i32.const 0))
              (func $sniff (export "sniff") (param i32 i32 i32 i32) (result i64)
                (loop $forever (br $forever))
                (i64.const 0)))
        "#;
        let engine = epoch_engine();
        // Fast ticker so the test does not hang.
        {
            let e = engine.clone();
            std::thread::spawn(move || loop {
                std::thread::sleep(Duration::from_millis(5));
                e.increment_epoch();
            });
        }
        let sniffer = wasm_sniffer("looper", wat, &engine);
        // Should return None (timeout counted as an error, not a hang) rather
        // than block forever.
        assert!(sniffer.sniff(b"anything").is_none());
    }

    #[test]
    fn short_calls_never_time_out_under_the_shipped_ticker() {
        // Regression (#173): a one-tick deadline traps at the *next* tick,
        // anywhere from 0 to `call_timeout` after the call started, so a call
        // far under the timeout was still killed ~duration/timeout of the time.
        let wat = r#"
            (module
              (memory (export "memory") 2)
              (func $alloc (export "alloc") (param i32) (result i32) (i32.const 0))
              (func $sniff (export "sniff") (param i32 i32 i32 i32) (result i64)
                (local $i i32)
                (loop $spin
                  (local.set $i (i32.add (local.get $i) (i32.const 1)))
                  (br_if $spin (i32.lt_u (local.get $i) (i32.const 1000000))))
                (i32.store8 (i32.const 65536) (i32.const 0x02))
                (i32.store8 (i32.const 65537) (i32.const 1))
                (i32.store8 (i32.const 65538) (i32.const 0))
                (i32.store8 (i32.const 65539) (i32.const 0x61))
                (i64.or (i64.shl (i64.const 65536) (i64.const 32)) (i64.const 4))))
        "#;
        let loader = SnifferLoader::new(Duration::from_millis(20)).unwrap();
        let sniffer = wasm_sniffer("spinner", wat, &loader.engine);
        let started = std::time::Instant::now();
        let mut timed_out = 0;
        let mut calls = 0;
        while calls < 1500 {
            if matches!(sniffer.call(b"x"), Err(CallError::Timeout)) {
                timed_out += 1;
            }
            calls += 1;
        }
        let per_call = started.elapsed() / calls;
        assert!(
            per_call < Duration::from_millis(5),
            "test premise: each call must be far under the 20 ms timeout, was {per_call:?}"
        );
        assert_eq!(timed_out, 0, "{timed_out}/{calls} short calls timed out");
    }

    #[test]
    fn build_sniffers_loads_wasm_files_from_dir() {
        let dir = tempdir();
        std::fs::write(
            dir.join("test-host.wasm"),
            wat::parse_str(HOST_SNIFFER_WAT).unwrap(),
        )
        .unwrap();
        let (_loader, reg) = build_sniffers(&cfg(&dir)).unwrap();
        let s = reg.get("test-host").unwrap();
        assert_eq!(s.sniff(b"HOST:x").unwrap().host.as_deref(), Some("x"));
    }

    #[test]
    fn build_sniffers_enforces_module_pins() {
        let dir = tempdir();
        let bytes = wat::parse_str(HOST_SNIFFER_WAT).unwrap();
        std::fs::write(dir.join("test-host.wasm"), &bytes).unwrap();

        let mut c = cfg(&dir);
        c.modules.push(wayhouse_config::SnifferModulePin {
            name: "test-host".into(),
            sha256: "0".repeat(64),
            config: None,
        });
        assert!(build_sniffers(&c).is_err(), "wrong pin must fail the load");

        let mut c = cfg(&dir);
        c.modules.push(wayhouse_config::SnifferModulePin {
            name: "test-host".into(),
            sha256: format!("{:x}", Sha256::digest(&bytes)),
            config: None,
        });
        assert!(build_sniffers(&c).is_ok(), "correct pin must load");
    }

    #[test]
    fn wasm_plugin_receives_its_config() {
        let engine = epoch_engine();

        let with_cfg = wasm_sniffer_cfg("cfg-echo", CFG_ECHO_WAT, &engine, b"eu-west".to_vec());
        let hint = with_cfg
            .sniff(b"whatever")
            .expect("config should be delivered");
        assert_eq!(hint.host.as_deref(), Some("eu-west"));

        // No configured `config` ⇒ the guest sees `cfg_len == 0`.
        let no_cfg = wasm_sniffer_cfg("cfg-echo", CFG_ECHO_WAT, &engine, Vec::new());
        assert!(no_cfg.sniff(b"whatever").is_none());
    }

    #[test]
    fn build_sniffers_wires_module_config_through_the_scan() {
        let dir = tempdir();
        let bytes = wat::parse_str(CFG_ECHO_WAT).unwrap();
        std::fs::write(dir.join("cfg-echo.wasm"), &bytes).unwrap();

        let mut c = cfg(&dir);
        c.modules.push(wayhouse_config::SnifferModulePin {
            name: "cfg-echo".into(),
            sha256: format!("{:x}", Sha256::digest(&bytes)),
            config: Some("lobby-a".into()),
        });
        let (_loader, reg) = build_sniffers(&c).unwrap();
        let hint = reg.get("cfg-echo").unwrap().sniff(b"x").unwrap();
        assert_eq!(hint.host.as_deref(), Some("lobby-a"));
    }

    #[test]
    fn scan_reflects_added_and_removed_modules() {
        let dir = tempdir();
        std::fs::write(
            dir.join("test-host.wasm"),
            wat::parse_str(HOST_SNIFFER_WAT).unwrap(),
        )
        .unwrap();
        let (loader, registry) = build_sniffers(&cfg(&dir)).unwrap();
        assert!(registry.get("test-host").is_some());

        // A second module appears; scan (as a reload rescan would) and apply
        // the result the same way the reload task does.
        std::fs::write(
            dir.join("other.wasm"),
            wat::parse_str(HOST_SNIFFER_WAT).unwrap(),
        )
        .unwrap();
        let map = loader.scan(&cfg(&dir)).unwrap();
        assert_eq!(map.len(), 2);
        registry.replace(map);
        assert!(registry.get("test-host").is_some());
        assert!(registry.get("other").is_some());

        // Removing a file and rescanning drops it from the next map.
        std::fs::remove_file(dir.join("other.wasm")).unwrap();
        let map = loader.scan(&cfg(&dir)).unwrap();
        assert_eq!(map.len(), 1);
        registry.replace(map);
        assert!(registry.get("test-host").is_some());
        assert!(registry.get("other").is_none());
    }

    /// A RakNet Unconnected Ping: id, 8-byte time, the offline magic, client GUID.
    fn raknet_ping() -> Vec<u8> {
        let mut p = vec![0x01u8; 1];
        p.extend_from_slice(&[0; 8]);
        p.extend_from_slice(&[
            0x00, 0xff, 0xff, 0x00, 0xfe, 0xfe, 0xfe, 0xfe, 0xfd, 0xfd, 0xfd, 0xfd, 0x12, 0x34,
            0x56, 0x78,
        ]);
        p.extend_from_slice(&[7; 8]);
        p
    }

    /// A TeamSpeak 3 init step 0 packet.
    fn ts3_init() -> Vec<u8> {
        let mut p = b"TS3INIT1".to_vec();
        p.extend_from_slice(&[0, 101, 0, 0, 0x88, 6, 0x3b, 0xec, 0xe9, 0]);
        p.extend_from_slice(&[0; 16]);
        p
    }

    /// Every real built plugin must carry the host's ABI version in its
    /// `wayhouse.abi` section — this is what catches the section being dropped by
    /// `strip`/`lto` in the plugins release profile, and the host and guest
    /// constants drifting apart. Needs `make plugins`; run with
    /// `cargo test -p wayhouse built_plugins_declare -- --ignored`.
    #[test]
    #[ignore = "needs `make plugins` to have built crates/plugins first"]
    fn built_plugins_declare_the_host_abi() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../plugins/target/wasm32-unknown-unknown/release");
        assert!(
            dir.is_dir(),
            "run `make plugins` first (looked in {})",
            dir.display()
        );
        let mut found = 0;
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "wasm") {
                let bytes = std::fs::read(&path).unwrap();
                assert_eq!(
                    read_abi_version(&bytes).unwrap_or_else(|e| panic!("{}: {e}", path.display())),
                    HOST_ABI,
                    "{}",
                    path.display()
                );
                found += 1;
            }
        }
        assert!(
            found >= 8,
            "expected the 8 first-party plugins, found {found}"
        );
    }

    /// Loads the real first-party plugins (`crates/plugins/`) built by
    /// `make plugins` and drives each one through this crate's own loader —
    /// not just the plugin's own native `recognise()` unit tests, but the
    /// actual `alloc`/`memory.write`/`sniff`/decode round trip through
    /// `wasmtime`. Ignored by default: it needs
    /// `crates/plugins/target/wasm32-unknown-unknown/release/*.wasm` to
    /// exist, which `cargo test -p wayhouse` alone does not build. Run with
    /// `cargo test -p wayhouse plugin_artifacts -- --ignored` after `make plugins`.
    #[test]
    #[ignore = "needs `make plugins` to have built crates/plugins first"]
    fn first_party_plugin_artifacts_recognise_their_protocols() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../plugins/target/wasm32-unknown-unknown/release");
        assert!(
            dir.is_dir(),
            "run `make plugins` first (looked in {})",
            dir.display()
        );
        // A real plugin's std allocator wants more than the 1 MiB `cfg()`
        // gives synthetic WAT fixtures above — use the config default.
        let mut sc = cfg(&dir);
        sc.max_memory_bytes = 16 * 1024 * 1024;
        // A long runway, as in `wasm_boundary_latency_vs_nfr_n1`: a call that
        // straddles an epoch tick legitimately times out, and on a busy CI
        // runner a 50 ms tick made this test fail now and then. Timeouts have
        // their own test (`wasm_plugin_call_times_out_under_the_epoch_deadline`).
        sc.call_timeout = Duration::from_secs(10);
        let (_loader, registry) = build_sniffers(&sc).unwrap();

        let a2s = registry.get("a2s").expect("a2s.wasm not built");
        let hint = a2s
            .sniff(b"\xff\xff\xff\xffTSource Engine Query\0")
            .unwrap();
        assert_eq!(hint.key.as_deref(), Some("a2s"));
        assert!(a2s.sniff(b"not a2s at all").is_none());

        let quic = registry.get("quic").expect("quic.wasm not built");
        // A v1 Initial: long header, version 1, 8-byte DCID, 4-byte SCID.
        let mut initial = vec![0xc0, 0, 0, 0, 1, 8];
        initial.extend_from_slice(&[7; 8]);
        initial.push(4);
        initial.extend_from_slice(&[9; 4]);
        initial.extend_from_slice(&[0; 32]);
        assert_eq!(quic.sniff(&initial).unwrap().key.as_deref(), Some("quic"));
        assert!(quic.sniff(b"not quic at all").is_none());
        // A real client Initial (aioquic, v1): the plugin decrypts it and
        // reports the SNI as the host.
        let real: Vec<u8> = include_str!("../../plugins/quic/testdata/v1_mixed_case.hex")
            .trim()
            .as_bytes()
            .chunks(2)
            .map(|c| u8::from_str_radix(std::str::from_utf8(c).unwrap(), 16).unwrap())
            .collect();
        let hint = quic.sniff(&real).unwrap();
        assert_eq!(hint.key.as_deref(), Some("quic"));
        assert_eq!(hint.host.as_deref(), Some("play.example.net"));

        let wireguard = registry.get("wireguard").expect("wireguard.wasm not built");
        let mut initiation = vec![0u8; 148];
        initiation[0] = 1;
        assert_eq!(
            wireguard.sniff(&initiation).unwrap().key.as_deref(),
            Some("wireguard")
        );
        assert!(wireguard.sniff(&initiation[..147]).is_none());

        let openvpn = registry.get("openvpn").expect("openvpn.wasm not built");
        let mut reset = vec![0x38u8];
        reset.extend_from_slice(&[1; 13]);
        assert_eq!(
            openvpn.sniff(&reset).unwrap().key.as_deref(),
            Some("openvpn")
        );
        assert!(openvpn.sniff(&reset[..13]).is_none());

        let raknet = registry.get("raknet").expect("raknet.wasm not built");
        let ping = raknet_ping();
        assert_eq!(raknet.sniff(&ping).unwrap().key.as_deref(), Some("raknet"));
        assert!(raknet.sniff(&ping[..24]).is_none());

        let ts3 = registry
            .get("teamspeak3")
            .expect("teamspeak3.wasm not built");
        let init = ts3_init();
        assert_eq!(ts3.sniff(&init).unwrap().key.as_deref(), Some("teamspeak3"));
        assert!(ts3.sniff(&init[..17]).is_none());

        let minecraft = registry.get("minecraft").expect("minecraft.wasm not built");
        // A minimal handshake: len, id=0x00, protocol=1, "play.example.net", port, next_state=1.
        let mut pkt = vec![0x00, 0x01];
        let host = b"play.example.net";
        pkt.push(host.len() as u8);
        pkt.extend_from_slice(host);
        pkt.extend_from_slice(&25565u16.to_be_bytes());
        pkt.push(0x01);
        let mut framed = vec![pkt.len() as u8];
        framed.extend(pkt);
        let hint = minecraft.sniff(&framed).unwrap();
        assert_eq!(hint.host.as_deref(), Some("play.example.net"));

        // `regex_firstbytes` needs a `config` to match anything — with none
        // (this `cfg()` has no `modules`) it recognises nothing.
        let rf = registry
            .get("regex_firstbytes")
            .expect("regex_firstbytes.wasm not built");
        assert!(rf.sniff(b"GET / HTTP/1.1\r\n").is_none());
    }

    /// `regex_firstbytes` driven by a real `modules[].config` through the whole
    /// loader path — proves the config-string plumbing (A2) plus the plugin's
    /// own pattern language (A3) agree end to end.
    #[test]
    #[ignore = "needs `make plugins` to have built crates/plugins first"]
    fn first_party_regex_firstbytes_matches_by_config() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../plugins/target/wasm32-unknown-unknown/release/regex_firstbytes.wasm");
        assert!(
            src.is_file(),
            "run `make plugins` first (looked for {})",
            src.display()
        );

        let dir = tempdir();
        let bytes = std::fs::read(&src).unwrap();
        std::fs::write(dir.join("regex_firstbytes.wasm"), &bytes).unwrap();

        let mut sc = cfg(&dir);
        sc.max_memory_bytes = 16 * 1024 * 1024;
        sc.call_timeout = Duration::from_secs(10); // see the test above
        sc.modules.push(wayhouse_config::SnifferModulePin {
            name: "regex_firstbytes".into(),
            sha256: format!("{:x}", Sha256::digest(&bytes)),
            config: Some("key:a2s|@0 hex:ffffffff|ascii:GET ".into()),
        });
        let (_loader, registry) = build_sniffers(&sc).unwrap();
        let rf = registry.get("regex_firstbytes").unwrap();

        assert_eq!(
            rf.sniff(b"\xff\xff\xff\xffTSource Engine Query\0")
                .unwrap()
                .key
                .as_deref(),
            Some("a2s"),
        );
        assert_eq!(
            rf.sniff(b"GET /health HTTP/1.1\r\n")
                .unwrap()
                .key
                .as_deref(),
            Some("a2s"),
        );
        assert!(rf.sniff(b"POST /x HTTP/1.1\r\n").is_none());
    }

    /// Phase 9 slice 6: bench the WASM boundary — a fresh `Store` + `Instance`
    /// per call, exactly the "no state survives between connections" design
    /// from slice 3 — against NFR N1 (< 0.5 ms *added* latency; see
    /// `docs/01-requirements.md`), per the locked "latency is a gate, not an
    /// assumption" decision in `docs/08` Phase 9. Same ignored-by-default
    /// convention as the artifact test above (needs `make plugins`); run with
    /// `cargo test -p wayhouse --release wasm_boundary -- --ignored --nocapture`
    /// to see the printed report (release matters here — `wasmtime`'s
    /// Cranelift compiler and the sandboxed call are both far slower
    /// unoptimised).
    #[test]
    #[ignore = "needs `make plugins` to have built crates/plugins first; run --release for real numbers"]
    #[allow(clippy::items_after_statements)] // test-local items sit next to their only use
    fn wasm_boundary_latency_vs_nfr_n1() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../plugins/target/wasm32-unknown-unknown/release");
        assert!(
            dir.is_dir(),
            "run `make plugins` first (looked in {})",
            dir.display()
        );
        let mut sc = cfg(&dir);
        sc.max_memory_bytes = 16 * 1024 * 1024;
        // A long call_timeout for this bench specifically: the epoch ticker
        // (slice 3) fires on wall-clock time shared across every plugin's
        // loop below, and a call that happens to straddle a tick boundary
        // legitimately traps — proven by
        // `wasm_plugin_call_times_out_under_the_epoch_deadline` already. That
        // mechanism is real and correct (and worth remembering: it means a
        // production `call_timeout_ms` is a *ceiling*, not a guarantee that
        // every call under it completes — a call started just before the
        // deadline gets less than the full budget). It just isn't what this
        // bench is measuring, so give it a long runway instead of coupling
        // the two.
        sc.call_timeout = std::time::Duration::from_secs(10);
        // Pin all three (a config on `regex_firstbytes` is what makes it match;
        // once `modules` is non-empty every file must be pinned).
        for name in [
            "a2s",
            "minecraft",
            "quic",
            "wireguard",
            "openvpn",
            "raknet",
            "teamspeak3",
            "regex_firstbytes",
        ] {
            let bytes = std::fs::read(dir.join(format!("{name}.wasm"))).unwrap();
            sc.modules.push(wayhouse_config::SnifferModulePin {
                name: name.into(),
                sha256: format!("{:x}", Sha256::digest(&bytes)),
                config: (name == "regex_firstbytes").then(|| "ascii:GET ".to_string()),
            });
        }
        let (_loader, registry) = build_sniffers(&sc).unwrap();

        // One representative, recognised payload per plugin — the
        // recognised path is the more expensive one (it also encodes and
        // decodes a `RouteHint`), so it's the one that matters for the gate.
        let minecraft_handshake = {
            let host = b"play.example.net";
            let mut pkt = vec![0x00u8, 0x01];
            pkt.push(host.len() as u8);
            pkt.extend_from_slice(host);
            pkt.extend_from_slice(&25565u16.to_be_bytes());
            pkt.push(0x01);
            let mut framed = vec![pkt.len() as u8];
            framed.extend(pkt);
            framed
        };
        let quic_initial = [
            &[0xc0u8, 0, 0, 0, 1, 8][..],
            &[7; 8],
            &[4],
            &[9; 4],
            &[0; 32],
        ]
        .concat();
        let mut wg_initiation = vec![0u8; 148];
        wg_initiation[0] = 1;
        let mut openvpn_reset = vec![0x38u8];
        openvpn_reset.extend_from_slice(&[1; 13]);
        let (raknet_ping, ts3_init) = (raknet_ping(), ts3_init());
        let cases: [(&str, &[u8]); 8] = [
            ("a2s", b"\xff\xff\xff\xffTSource Engine Query\0"),
            ("minecraft", &minecraft_handshake),
            ("quic", &quic_initial),
            ("wireguard", &wg_initiation),
            ("openvpn", &openvpn_reset),
            ("raknet", &raknet_ping),
            ("teamspeak3", &ts3_init),
            ("regex_firstbytes", b"GET /health HTTP/1.1\r\nHost: x\r\n"),
        ];

        const WARMUP: usize = 50;
        const ITERATIONS: usize = 2000;
        const N1_US: f64 = 500.0; // NFR N1: added p50 < 0.5 ms

        let mut all_pass = true;
        println!(
            "\n{:<18} {:>10} {:>10} {:>10} {:>10}  N1",
            "plugin", "p50 (us)", "p90 (us)", "p99 (us)", "max (us)"
        );
        for (name, payload) in cases {
            let sniffer = registry
                .get(name)
                .unwrap_or_else(|| panic!("{name}.wasm not built"));
            for _ in 0..WARMUP {
                assert!(
                    sniffer.sniff(payload).is_some(),
                    "{name} must recognise its own fixture"
                );
            }
            let mut samples = Vec::with_capacity(ITERATIONS);
            for _ in 0..ITERATIONS {
                let start = std::time::Instant::now();
                let hint = sniffer.sniff(payload);
                samples.push(start.elapsed());
                assert!(hint.is_some());
            }
            samples.sort_unstable();
            let us = |d: std::time::Duration| d.as_secs_f64() * 1e6;
            let at = |q: f64| samples[((ITERATIONS as f64 * q) as usize).min(ITERATIONS - 1)];
            let (p50, p90, p99, max) = (at(0.50), at(0.90), at(0.99), samples[ITERATIONS - 1]);
            let pass = us(p50) < N1_US;
            all_pass &= pass;
            println!(
                "{:<18} {:>10.1} {:>10.1} {:>10.1} {:>10.1}  {}",
                name,
                us(p50),
                us(p90),
                us(p99),
                us(max),
                if pass { "PASS" } else { "MISS" },
            );
        }
        println!(
            "({ITERATIONS} calls/plugin after {WARMUP} warmup, one fresh Store+Instance per \
             call, release build matters — debug is not representative)\n"
        );
        assert!(
            all_pass,
            "a plugin's median call exceeded NFR N1 (0.5ms) — see the InstancePre / \
             warm-instance fallbacks noted in docs/08 Phase 9 if this trips in a real run"
        );
    }

    /// Phase 9 slice 7: end to end through the *real* loader — not a native
    /// `Sniffer` impl handed to the registry directly (that's
    /// `wayhouse_core::sniff::tests::sniffer_matcher_routes_a_connection_by_hint_host`),
    /// but a `host-echo.wasm` compiled from [`HOST_SNIFFER_WAT`] by
    /// [`build_sniffers`] scanning a directory, exactly as `settings.
    /// sniffers.dir` would at real startup, then a live TCP connection routed
    /// by the hint that plugin returns. No `#[ignore]` needed — `wat::parse_str`
    /// builds the fixture inline, so this test needs neither
    /// `wasm32-unknown-unknown` nor `make plugins`.
    #[tokio::test]
    async fn end_to_end_connection_routes_by_a_real_wasm_plugins_hint() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::{TcpListener, TcpStream};

        let dir = tempdir();
        std::fs::write(
            dir.join("host-echo.wasm"),
            wat::parse_str(HOST_SNIFFER_WAT).unwrap(),
        )
        .unwrap();
        let (_loader, registry) = build_sniffers(&cfg(&dir)).unwrap();
        let sniffers = std::sync::Arc::new(registry);

        // Two backends, each writing an identifying byte on connect — same
        // shape as the native-sniffer test this mirrors.
        #[allow(clippy::items_after_statements)] // test-local items sit next to their only use
        async fn marker(tag: u8) -> std::net::SocketAddr {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = l.local_addr().unwrap();
            tokio::spawn(async move {
                while let Ok((mut s, _)) = l.accept().await {
                    tokio::spawn(async move {
                        let _ = s.write_all(&[tag]).await;
                        let mut buf = [0u8; 64];
                        while let Ok(n) = s.read(&mut buf).await {
                            if n == 0 {
                                break;
                            }
                        }
                    });
                }
            });
            addr
        }

        let survival = marker(b'S').await;
        let lobby = marker(b'L').await;
        let proxy = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap();

        let yaml = format!(
            r#"
pools:
  - name: survival
    targets: ["{survival}"]
  - name: lobby
    targets: ["{lobby}"]
listeners:
  - name: l
    bind: "{proxy}"
    routes:
      - match: {{ type: sniffer, sniffer: host-echo, host: ["survival.example.net"] }}
        action: {{ pool: survival }}
      - match: {{ type: always }}
        action: {{ pool: lobby }}
"#
        );
        let cfg = wayhouse_config::parse_str(&yaml).unwrap();
        let runtime = wayhouse_core::Runtime::start_with_sniffers(
            wayhouse_core::Snapshot::from_config(&cfg),
            Arc::default(),
            None,
            sniffers,
            1,
        );
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        let mark = |host: &'static str| async move {
            let mut c = TcpStream::connect(proxy).await.unwrap();
            // Unlike the native `TestHost` sniffer in `wayhouse_core::sniff::tests`
            // (which stops at the first `\n`), `HOST_SNIFFER_WAT` is a
            // minimal fixture that takes *everything after* `HOST:` as the
            // host — by design, see its doc comment. So this payload carries
            // no trailing bytes past the hostname.
            c.write_all(format!("HOST:{host}").as_bytes())
                .await
                .unwrap();
            let mut m = [0u8; 1];
            c.read_exact(&mut m).await.unwrap();
            m[0]
        };

        assert_eq!(mark("survival.example.net").await, b'S');
        assert_eq!(mark("creative.example.net").await, b'L');

        runtime
            .shutdown_with_grace(std::time::Duration::from_millis(100))
            .await;
    }

    fn loader() -> SnifferLoader {
        SnifferLoader::new(Duration::from_millis(50)).unwrap()
    }

    fn wat_bytes(wat: &str) -> Vec<u8> {
        wat::parse_str(wat).unwrap()
    }

    const MEM: usize = 1 << 20;

    #[test]
    fn validate_accepts_the_host_fixture() {
        loader()
            .validate(&wat_bytes(HOST_SNIFFER_WAT), MEM)
            .unwrap();
    }

    #[test]
    fn validate_rejects_empty() {
        assert!(matches!(
            loader().validate(&[], MEM),
            Err(ModuleError::Empty)
        ));
    }

    #[test]
    fn validate_rejects_garbage_bytes() {
        let e = loader().validate(b"not wasm", MEM).unwrap_err();
        assert!(matches!(e, ModuleError::Compile(_)), "{e}");
    }

    #[test]
    fn validate_rejects_oversize() {
        let big = vec![0u8; MAX_MODULE_BYTES + 1];
        let e = loader().validate(&big, MEM).unwrap_err();
        assert!(matches!(e, ModuleError::TooLarge { .. }), "{e}");
    }

    #[test]
    fn validate_rejects_import() {
        let wat = r#"(module
          (@custom "wayhouse.abi" "\00\00\01\00")
          (import "wasi_snapshot_preview1" "fd_write" (func))
          (memory (export "memory") 1)
          (func (export "alloc") (param i32) (result i32) (i32.const 0))
          (func (export "sniff") (param i32 i32 i32 i32) (result i64) (i64.const 0)))"#;
        let e = loader().validate(&wat_bytes(wat), MEM).unwrap_err();
        assert!(matches!(e, ModuleError::UnexpectedImport(_)), "{e}");
        assert!(e.to_string().contains("fd_write"), "{e}");
    }

    #[test]
    fn validate_rejects_missing_sniff() {
        let wat = r#"(module
          (@custom "wayhouse.abi" "\00\00\01\00")
          (memory (export "memory") 1)
          (func (export "alloc") (param i32) (result i32) (i32.const 0)))"#;
        let e = loader().validate(&wat_bytes(wat), MEM).unwrap_err();
        assert!(matches!(e, ModuleError::MissingExport("sniff")), "{e}");
    }

    #[test]
    fn validate_rejects_wrong_sniff_signature() {
        let wat = r#"(module
          (@custom "wayhouse.abi" "\00\00\01\00")
          (memory (export "memory") 1)
          (func (export "alloc") (param i32) (result i32) (i32.const 0))
          (func (export "sniff") (param i32) (result i64) (i64.const 0)))"#;
        let e = loader().validate(&wat_bytes(wat), MEM).unwrap_err();
        assert!(matches!(e, ModuleError::WrongSignature("sniff")), "{e}");
    }

    #[test]
    fn validate_rejects_start_function_that_traps() {
        let wat = r#"(module
          (@custom "wayhouse.abi" "\00\00\01\00")
          (memory (export "memory") 1)
          (func $f unreachable)
          (start $f)
          (func (export "alloc") (param i32) (result i32) (i32.const 0))
          (func (export "sniff") (param i32 i32 i32 i32) (result i64) (i64.const 0)))"#;
        let e = loader().validate(&wat_bytes(wat), MEM).unwrap_err();
        assert!(matches!(e, ModuleError::Instantiate(_)), "{e}");
    }

    /// A valid sniffer module with no ABI declaration.
    const BARE_WAT: &str = r#"(module
      (memory (export "memory") 1)
      (func (export "alloc") (param i32) (result i32) (i32.const 0))
      (func (export "sniff") (param i32 i32 i32 i32) (result i64) (i64.const 0)))"#;

    fn leb(mut n: usize) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let b = (n & 0x7f) as u8;
            n >>= 7;
            if n == 0 {
                out.push(b);
                return out;
            }
            out.push(b | 0x80);
        }
    }

    /// `bytes` with one custom section appended.
    fn with_custom(mut bytes: Vec<u8>, name: &str, payload: &[u8]) -> Vec<u8> {
        let mut body = leb(name.len());
        body.extend_from_slice(name.as_bytes());
        body.extend_from_slice(payload);
        bytes.push(0);
        bytes.extend(leb(body.len()));
        bytes.extend(body);
        bytes
    }

    fn with_abi(major: u16, minor: u16) -> Vec<u8> {
        let mut payload = major.to_le_bytes().to_vec();
        payload.extend(minor.to_le_bytes());
        with_custom(wat_bytes(BARE_WAT), "wayhouse.abi", &payload)
    }

    #[test]
    fn abi_reads_0_1() {
        let v = read_abi_version(&with_abi(0, 1)).unwrap();
        assert_eq!(v, AbiVersion { major: 0, minor: 1 });
        assert_eq!(v.to_string(), "0.1");
        assert_eq!(v, HOST_ABI);
    }

    #[test]
    fn abi_reads_the_bytes_little_endian() {
        let v = read_abi_version(&with_abi(0x0102, 0x0304)).unwrap();
        assert_eq!(
            v,
            AbiVersion {
                major: 0x0102,
                minor: 0x0304
            }
        );
    }

    #[test]
    fn abi_missing_is_rejected_with_hint() {
        let e = loader().validate(&wat_bytes(BARE_WAT), MEM).unwrap_err();
        assert!(matches!(e, ModuleError::AbiMissing), "{e}");
        let msg = e.to_string();
        assert!(msg.contains("wayhouse-sniffer-abi"), "{msg}");
        assert!(msg.contains("0.1"), "{msg}");
    }

    #[test]
    fn abi_payload_of_3_and_6_bytes_rejected() {
        for len in [3usize, 6] {
            let bytes = with_custom(wat_bytes(BARE_WAT), "wayhouse.abi", &vec![0u8; len]);
            let e = read_abi_version(&bytes).unwrap_err();
            assert!(matches!(e, ModuleError::AbiMalformed(n) if n == len), "{e}");
        }
    }

    #[test]
    fn abi_two_sections_rejected() {
        let bytes = with_custom(with_abi(0, 1), "wayhouse.abi", &[0, 0, 1, 0]);
        let e = read_abi_version(&bytes).unwrap_err();
        assert!(matches!(e, ModuleError::AbiMalformed(_)), "{e}");
    }

    #[test]
    fn abi_other_minor_rejected_while_major_is_0() {
        for minor in [0u16, 2] {
            let e = loader().validate(&with_abi(0, minor), MEM).unwrap_err();
            assert!(
                matches!(e, ModuleError::AbiIncompatible { plugin, host }
                    if plugin == AbiVersion { major: 0, minor } && host == HOST_ABI),
                "{e}"
            );
            let msg = e.to_string();
            assert!(
                msg.contains(&format!("0.{minor}")) && msg.contains("0.1"),
                "{msg}"
            );
        }
    }

    #[test]
    fn abi_other_major_rejected() {
        let e = loader().validate(&with_abi(1, 1), MEM).unwrap_err();
        assert!(matches!(e, ModuleError::AbiIncompatible { .. }), "{e}");
    }

    #[test]
    fn abi_extra_custom_sections_ignored() {
        let bytes = with_custom(with_abi(0, 1), "name", b"\x00");
        let bytes = with_custom(bytes, "producers", b"rustc");
        assert_eq!(read_abi_version(&bytes).unwrap(), HOST_ABI);
        loader().validate(&bytes, MEM).unwrap();
    }

    #[test]
    fn abi_read_does_not_need_a_valid_body() {
        assert!(matches!(
            read_abi_version(b"not wasm"),
            Err(ModuleError::Compile(_))
        ));
    }

    #[test]
    fn scan_skips_module_without_abi() {
        let dir = tempdir();
        std::fs::write(dir.join("good.wasm"), wat_bytes(HOST_SNIFFER_WAT)).unwrap();
        std::fs::write(dir.join("old.wasm"), wat_bytes(BARE_WAT)).unwrap();
        let map = loader().scan(&cfg(&dir)).unwrap();
        assert_eq!(map.keys().cloned().collect::<Vec<_>>(), ["good"]);
    }

    #[test]
    fn scan_skips_garbage_file_and_loads_the_rest() {
        let dir = tempdir();
        std::fs::write(dir.join("good.wasm"), wat_bytes(HOST_SNIFFER_WAT)).unwrap();
        std::fs::write(dir.join("junk.wasm"), b"not wasm").unwrap();
        let map = loader().scan(&cfg(&dir)).unwrap();
        let mut names: Vec<_> = map.keys().cloned().collect();
        names.sort();
        assert_eq!(names, ["good"]);
    }

    #[test]
    fn scan_skips_module_with_import() {
        let dir = tempdir();
        std::fs::write(dir.join("good.wasm"), wat_bytes(HOST_SNIFFER_WAT)).unwrap();
        let wat = r#"(module (import "env" "x" (func))
          (@custom "wayhouse.abi" "\00\00\01\00")
          (memory (export "memory") 1)
          (func (export "alloc") (param i32) (result i32) (i32.const 0))
          (func (export "sniff") (param i32 i32 i32 i32) (result i64) (i64.const 0)))"#;
        std::fs::write(dir.join("imp.wasm"), wat_bytes(wat)).unwrap();
        let map = loader().scan(&cfg(&dir)).unwrap();
        assert_eq!(map.len(), 1);
        assert!(map.contains_key("good"));
    }

    #[test]
    fn scan_still_fails_on_pin_mismatch() {
        let dir = tempdir();
        std::fs::write(dir.join("good.wasm"), wat_bytes(HOST_SNIFFER_WAT)).unwrap();
        let mut c = cfg(&dir);
        c.modules.push(wayhouse_config::SnifferModulePin {
            name: "good".into(),
            sha256: "0".repeat(64),
            config: None,
        });
        assert!(loader().scan(&c).is_err());
    }

    /// Pin `bytes` as `name` in `c` (matching sha256, so only validation can fail).
    fn pin(c: &mut SniffersConfig, name: &str, bytes: &[u8]) {
        c.modules.push(wayhouse_config::SnifferModulePin {
            name: name.into(),
            sha256: format!("{:x}", Sha256::digest(bytes)),
            config: None,
        });
    }

    const IMPORTING_WAT: &str = r#"(module (import "env" "x" (func))
          (@custom "wayhouse.abi" "\00\00\01\00")
          (memory (export "memory") 1)
          (func (export "alloc") (param i32) (result i32) (i32.const 0))
          (func (export "sniff") (param i32 i32 i32 i32) (result i64) (i64.const 0)))"#;

    #[test]
    fn scan_fails_on_pinned_module_without_abi() {
        let dir = tempdir();
        let bytes = wat_bytes(BARE_WAT);
        std::fs::write(dir.join("old.wasm"), &bytes).unwrap();
        let mut c = cfg(&dir);
        pin(&mut c, "old", &bytes);
        let e = loader()
            .scan(&c)
            .err()
            .expect("pinned invalid module must fail the scan");
        assert!(e.to_string().contains("old"), "error names the module: {e}");
    }

    #[test]
    fn scan_fails_on_pinned_module_with_import() {
        let dir = tempdir();
        let bytes = wat_bytes(IMPORTING_WAT);
        std::fs::write(dir.join("imp.wasm"), &bytes).unwrap();
        let mut c = cfg(&dir);
        pin(&mut c, "imp", &bytes);
        let e = loader()
            .scan(&c)
            .err()
            .expect("pinned invalid module must fail the scan");
        assert!(e.to_string().contains("imp"), "error names the module: {e}");
    }

    #[test]
    fn scan_with_a_valid_pinned_module_still_loads() {
        let dir = tempdir();
        let bytes = wat_bytes(HOST_SNIFFER_WAT);
        std::fs::write(dir.join("good.wasm"), &bytes).unwrap();
        let mut c = cfg(&dir);
        pin(&mut c, "good", &bytes);
        assert_eq!(loader().scan(&c).unwrap().len(), 1);
    }

    /// The reload path (`rescan_sniffers`) keeps the last good registry when a
    /// pinned module stops validating, and picks up a repaired one.
    #[test]
    fn reload_keeps_the_last_good_registry_when_a_pinned_module_breaks() {
        let dir = tempdir();
        let good = wat_bytes(HOST_SNIFFER_WAT);
        std::fs::write(dir.join("good.wasm"), &good).unwrap();
        let l = loader();
        let mut c = cfg(&dir);
        pin(&mut c, "good", &good);
        let mut config = wayhouse_config::parse_str(
            "pools:\n  - name: p\n    targets: [\"127.0.0.1:9\"]\n    health_check:\n      type: none\n\
             listeners:\n  - name: l1\n    bind: \"127.0.0.1:0\"\n    pool: p\n",
        )
        .unwrap();
        config.sniffers = Some(c.clone());
        let sniffers = Sniffers::default();
        crate::reload::rescan_sniffers(&config, Some(&l), &sniffers);
        assert_eq!(sniffers.names(), ["good"]);

        // The pinned file is replaced by an ABI-less module, re-pinned so
        // only validation fails.
        let bad = wat_bytes(BARE_WAT);
        std::fs::write(dir.join("good.wasm"), &bad).unwrap();
        let mut c2 = cfg(&dir);
        pin(&mut c2, "good", &bad);
        config.sniffers = Some(c2);
        crate::reload::rescan_sniffers(&config, Some(&l), &sniffers);
        assert_eq!(sniffers.names(), ["good"], "the previous registry is kept");
        assert!(
            sniffers.get("good").is_some(),
            "the previous plugin still resolves"
        );
    }

    #[test]
    fn scan_records_the_live_pins() {
        let dir = tempdir();
        let l = loader();
        assert!(l.pins().is_empty());
        let mut c = cfg(&dir);
        c.modules.push(wayhouse_config::SnifferModulePin {
            name: "x".into(),
            sha256: "1".repeat(64),
            config: None,
        });
        let _ = l.scan(&c);
        assert_eq!(l.pins().len(), 1);
        let _ = l.scan(&cfg(&dir));
        assert!(l.pins().is_empty(), "a reload that drops the pins is seen");
    }

    /// A tiny per-test-process unique scratch dir under the system temp dir.
    fn tempdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "wayhouse-sniffer-loader-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
