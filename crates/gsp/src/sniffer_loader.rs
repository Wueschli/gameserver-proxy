//! Phase 9 slice 3: the WASM sniffer plugin loader.
//!
//! Loads `*.wasm` modules from `settings.sniffers.dir` into a
//! [`gsp_core::sniff::Sniffers`] registry. Each module is a **core WASM
//! module — no WASI, no host imports**: a plugin cannot touch the filesystem,
//! the clock, or the network; it only ever sees the bytes it's handed and
//! returns a result. `wasmtime` is a binary-only dependency (like `reqwest`)
//! — `gsp-core` has no sandboxing dependency, only the `Sniffer` trait /
//! `Sniffers` registry it calls into.
//!
//! ## ABI
//! The guest exports `memory`, `alloc(len: i32) -> i32` (returns a pointer to
//! `len` free, host-writable bytes) and `sniff(ptr: i32, len: i32) -> i64` —
//! `0` means "not recognised"; any other value is the packed
//! `(result_ptr << 32) | result_len`, pointing at a compact encoding of a
//! `RouteHint`:
//!
//! ```text
//! byte 0:            flags (bit0 = reject, bit1 = host present, bit2 = key present)
//! if host present:    u16 LE length, then that many UTF-8 bytes
//! if key present:     u16 LE length, then that many UTF-8 bytes
//! ```
//!
//! This encoding is also what the (future, phase 9 slice 5) `gsp-sniffer-abi`
//! guest helper crate implements on the write side.
//!
//! ## Bounds
//! Two independent bounds keep a plugin from stalling or ballooning the
//! process: a shared `wasmtime::Engine` with epoch interruption — a ticker
//! thread bumps the engine's epoch every `call_timeout_ms`, and every call
//! sets a one-tick deadline, so a call that hasn't returned by the next tick
//! traps — and a per-call `StoreLimits` memory cap (`max_memory_bytes`). Every
//! call gets a fresh `Store` + `Instance`; no state survives between
//! connections (deliberate: simplicity over instance-reuse latency, revisited
//! in slice 6 if the per-call instantiate cost misses NFR N1).

use std::collections::HashMap;
use std::fs;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use wasmtime::{Config, Engine, Instance, Module, Store, StoreLimits, StoreLimitsBuilder};

use gsp_config::{RouteHint, SniffersConfig};
use gsp_core::metrics_defs as m;
use gsp_core::sniff::{Sniffer, Sniffers};

struct StoreState {
    limits: StoreLimits,
}

/// One loaded plugin module, ready to be instantiated per call.
pub struct WasmSniffer {
    name: String,
    engine: Arc<Engine>,
    module: Module,
    max_memory_bytes: usize,
}

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
        let state = StoreState {
            limits: StoreLimitsBuilder::new()
                .memory_size(self.max_memory_bytes)
                .build(),
        };
        let mut store = Store::new(&self.engine, state);
        store.limiter(|s| &mut s.limits);
        store.set_epoch_deadline(1);

        let instance = Instance::new(&mut store, &self.module, &[]).map_err(classify)?;
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or(CallError::Trap)?;
        let alloc = instance
            .get_typed_func::<i32, i32>(&mut store, "alloc")
            .map_err(|_| CallError::Trap)?;
        let sniff = instance
            .get_typed_func::<(i32, i32), i64>(&mut store, "sniff")
            .map_err(|_| CallError::Trap)?;

        let len = i32::try_from(first.len()).map_err(|_| CallError::BadOutput)?;
        let ptr = alloc.call(&mut store, len).map_err(classify)?;
        memory
            .write(&mut store, ptr as usize, first)
            .map_err(|_| CallError::BadOutput)?;

        let packed = sniff.call(&mut store, (ptr, len)).map_err(classify)?;
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
}

impl SnifferLoader {
    /// Build the engine and spawn its epoch-ticker thread (bumps the epoch
    /// every `call_timeout`, forever — one thread for the process, not one
    /// per plugin or per call).
    pub fn new(call_timeout: std::time::Duration) -> Result<Self> {
        let mut engine_cfg = Config::new();
        engine_cfg.epoch_interruption(true);
        let engine = Arc::new(
            Engine::new(&engine_cfg)
                .map_err(|e| anyhow::anyhow!("building the sniffer wasm engine: {e}"))?,
        );
        {
            let engine = engine.clone();
            std::thread::Builder::new()
                .name("gsp-sniffer-epoch".into())
                .spawn(move || loop {
                    std::thread::sleep(call_timeout);
                    engine.increment_epoch();
                })
                .context("spawning the sniffer epoch-ticker thread")?;
        }
        Ok(Self { engine })
    }

    /// Scan `cfg.dir` for `*.wasm` modules and compile each into a
    /// [`WasmSniffer`], returning the resulting name→plugin map. A module is
    /// named after its file stem (`a2s.wasm` → sniffer `a2s`). When
    /// `cfg.modules` is non-empty every loaded file must have a matching pin
    /// (`name` + `sha256`) — an unpinned or hash-mismatched file fails the
    /// whole scan (an old, still-pinned registry should be kept by the
    /// caller rather than left half-updated).
    pub fn scan(&self, cfg: &SniffersConfig) -> Result<HashMap<String, Arc<dyn Sniffer>>> {
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

            if !cfg.modules.is_empty() {
                let digest = format!("{:x}", Sha256::digest(&bytes));
                match cfg.modules.iter().find(|m| m.name == name) {
                    Some(pin) if pin.sha256 == digest => {}
                    Some(pin) => bail!(
                        "sniffer {name}: sha256 mismatch (pinned {}, loaded {digest})",
                        pin.sha256
                    ),
                    None => bail!("sniffer {name}: not listed in settings.sniffers.modules"),
                }
            }

            let module = Module::new(&self.engine, &bytes)
                .map_err(|e| anyhow::anyhow!("compiling {}: {e}", path.display()))?;
            let sniffer: Arc<dyn Sniffer> = Arc::new(WasmSniffer {
                name: name.clone(),
                engine: self.engine.clone(),
                module,
                max_memory_bytes: cfg.max_memory_bytes,
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
    /// `gsp_core::sniff::tests::TestHost`, hand-written in WAT so the test
    /// needs no `wasm32-unknown-unknown` toolchain. It walks the input byte
    /// by byte looking for a `\n`, treats everything after `HOST:` (5 bytes)
    /// as the host, and encodes a `RouteHint { host: Some(..), .. }` at a
    /// fixed output offset.
    const HOST_SNIFFER_WAT: &str = r#"
        (module
          (memory (export "memory") 2)
          ;; Bump allocator: next free offset lives at address 0 (reserved).
          (global $next (mut i32) (i32.const 4))
          (func $alloc (export "alloc") (param $len i32) (result i32)
            (local $p i32)
            (local.set $p (global.get $next))
            (global.set $next (i32.add (local.get $p) (local.get $len)))
            (local.get $p))

          ;; sniff(ptr, len) -> packed (out_ptr << 32 | out_len), 0 if no
          ;; "HOST:" prefix (first 5 bytes).
          (func $sniff (export "sniff") (param $ptr i32) (param $len i32) (result i64)
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

    fn wasm_sniffer(name: &str, wat: &str, engine: &Arc<Engine>) -> WasmSniffer {
        let bytes = wat::parse_str(wat).unwrap();
        let module = Module::new(engine, &bytes).unwrap();
        WasmSniffer {
            name: name.to_string(),
            engine: engine.clone(),
            module,
            max_memory_bytes: 1 << 20,
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
              (func $sniff (export "sniff") (param i32 i32) (result i64)
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
        c.modules.push(gsp_config::SnifferModulePin {
            name: "test-host".into(),
            sha256: "0".repeat(64),
        });
        assert!(build_sniffers(&c).is_err(), "wrong pin must fail the load");

        let mut c = cfg(&dir);
        c.modules.push(gsp_config::SnifferModulePin {
            name: "test-host".into(),
            sha256: format!("{:x}", Sha256::digest(&bytes)),
        });
        assert!(build_sniffers(&c).is_ok(), "correct pin must load");
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

    /// A tiny per-test-process unique scratch dir under the system temp dir.
    fn tempdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gsp-sniffer-loader-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
