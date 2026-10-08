//! Compiling plugins and running their entry points under limits.
//!
//! Every call gets a fresh `Store` and `Instance`; nothing survives in guest memory
//! between calls. A call's effects (state writes, log lines) are returned as a value and
//! never applied here: the caller commits them as one entry, which is what lets the
//! controller tag the entry with the leader term and reject it when stale.

use std::collections::BTreeMap;
use std::sync::{Arc, Weak};
use std::time::Duration;

use anyhow::Context;
use wasmtime::{
    Caller, Config, Engine, Extern, ExternType, FuncType, Instance, Linker, Memory, Module, Store,
    StoreLimits, StoreLimitsBuilder, Trap, ValType,
};

use crate::caps::Capabilities;
use crate::module::{inspect, ModuleError, ModuleInfo};

/// Epoch ticks a call may span before it traps. The ticker runs at
/// `call_timeout / EPOCH_DEADLINE_TICKS`, so a call gets between `call_timeout / 2` and
/// `call_timeout` (the same scheme as the sniffer loader).
const EPOCH_DEADLINE_TICKS: u64 = 2;
/// Longest state key the host accepts.
pub const MAX_KEY_BYTES: usize = 256;
/// Longest log line kept; longer ones are truncated.
pub const MAX_LOG_LINE_BYTES: usize = 1024;

/// A plugin's persisted state as the call starts.
pub type StateSnapshot = BTreeMap<String, Vec<u8>>;

/// Per-call limits.
#[derive(Debug, Clone)]
pub struct Limits {
    pub call_timeout: Duration,
    pub max_memory_bytes: usize,
    /// Elements a guest table may hold, at instantiation and when it grows.
    pub max_table_elements: usize,
    /// Structural limits checked before compiling.
    pub bounds: crate::bounds::Bounds,
    /// Log lines kept per call; further lines are counted in [`Effects::logs_dropped`].
    pub max_log_lines: usize,
    /// `state_put` calls accepted per call; further puts return `-1`.
    pub max_state_ops: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            call_timeout: Duration::from_secs(5),
            max_memory_bytes: 64 * 1024 * 1024,
            max_table_elements: 10_000,
            bounds: crate::bounds::Bounds::default(),
            max_log_lines: 64,
            max_state_ops: 256,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub level: LogLevel,
    pub msg: String,
}

/// What one call did, for the caller to commit.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Effects {
    pub state_puts: BTreeMap<String, Vec<u8>>,
    pub logs: Vec<LogLine>,
    pub logs_dropped: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum CallError {
    #[error("the call ran past its time limit")]
    Timeout,
    #[error("the call used the `{0}` capability, which was not approved")]
    CapabilityDenied(&'static str),
    #[error("the call trapped: {0}")]
    Trap(String),
}

struct CallState {
    limits: StoreLimits,
    caps: crate::caps::Capabilities,
    run: Limits,
    snapshot: Arc<StateSnapshot>,
    puts: BTreeMap<String, Vec<u8>>,
    /// Sum of key and value bytes of the merged (snapshot plus puts) state.
    state_bytes: usize,
    state_ops: usize,
    logs: Vec<LogLine>,
    logs_dropped: usize,
    denied: Option<&'static str>,
}

impl CallState {
    fn current(&self, key: &str) -> Option<&Vec<u8>> {
        self.puts.get(key).or_else(|| self.snapshot.get(key))
    }
}

/// The shared engine and its epoch ticker.
pub struct PluginHost {
    engine: Engine,
    limits: Limits,
    /// The ticker thread runs while any clone of this handle (host or plugin) lives.
    alive: Arc<()>,
}

impl PluginHost {
    /// Build the engine and spawn its epoch-ticker thread.
    pub fn new(limits: Limits) -> anyhow::Result<Self> {
        let mut cfg = Config::new();
        cfg.epoch_interruption(true);
        let engine =
            Engine::new(&cfg).map_err(|e| anyhow::anyhow!("building the plugin engine: {e}"))?;
        let tick = limits.call_timeout / u32::try_from(EPOCH_DEADLINE_TICKS).unwrap_or(2);
        let alive = Arc::new(());
        let watch: Weak<()> = Arc::downgrade(&alive);
        let ticker = engine.clone();
        std::thread::Builder::new()
            .name("wayhouse-plugin-epoch".into())
            .spawn(move || {
                while watch.upgrade().is_some() {
                    std::thread::sleep(tick);
                    ticker.increment_epoch();
                }
            })
            .context("spawning the plugin epoch-ticker thread")?;
        Ok(Self {
            engine,
            limits,
            alive,
        })
    }

    /// Validate and compile `bytes`: size, ABI, capabilities, imports, exports, and a
    /// trial instantiation under the call limits. No plugin entry point is called.
    ///
    /// `approved` is what the operator approved for this module's sha256. The module's
    /// own declaration must be within it, otherwise loading fails: a module cannot grant
    /// itself capabilities. The plugin then runs with its declaration, which is never
    /// wider than `approved`.
    pub fn load(&self, bytes: &[u8], approved: &Capabilities) -> Result<Plugin, ModuleError> {
        let info = inspect(bytes)?;
        info.caps.check_within(approved)?;
        crate::bounds::check(bytes, &self.limits.bounds)?;
        let module =
            Module::new(&self.engine, bytes).map_err(|e| ModuleError::Compile(e.to_string()))?;
        for i in module.imports() {
            let known =
                i.module() == "wayhouse" && matches!(i.name(), "log" | "state_get" | "state_put");
            if !known {
                return Err(ModuleError::UnexpectedImport(format!(
                    "{}::{}",
                    i.module(),
                    i.name()
                )));
            }
        }
        match module.get_export("memory") {
            Some(ExternType::Memory(_)) => {}
            Some(_) => return Err(ModuleError::WrongSignature("memory")),
            None => return Err(ModuleError::MissingExport("memory")),
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
        want("alloc", &[ValType::I32], &[ValType::I32])?;
        want("init", &[ValType::I32, ValType::I32], &[])?;
        if info.caps.triggers.on_timer {
            want("on_timer", &[], &[])?;
        }
        let plugin = Plugin {
            engine: self.engine.clone(),
            module,
            linker: linker(&self.engine),
            info,
            limits: self.limits.clone(),
            _alive: self.alive.clone(),
        };
        let mut store = plugin.store(Arc::default());
        plugin
            .linker
            .instantiate(&mut store, &plugin.module)
            .map_err(|e| ModuleError::Instantiate(e.to_string()))?;
        Ok(plugin)
    }
}

/// A loaded plugin, ready to be instantiated per call.
pub struct Plugin {
    engine: Engine,
    module: Module,
    linker: Linker<CallState>,
    info: ModuleInfo,
    limits: Limits,
    _alive: Arc<()>,
}

impl Plugin {
    pub fn info(&self) -> &ModuleInfo {
        &self.info
    }

    /// Call `init(config)`.
    pub fn init(&self, config: &[u8], state: &StateSnapshot) -> Result<Effects, CallError> {
        self.run(state, |store, inst| {
            let memory = guest_memory(store, inst)?;
            let (ptr, len) = if config.is_empty() {
                (0, 0)
            } else {
                let alloc = inst.get_typed_func::<i32, i32>(&mut *store, "alloc")?;
                let len = i32::try_from(config.len())?;
                let ptr = alloc.call(&mut *store, len)?;
                if ptr == 0 {
                    wasmtime::bail!("alloc returned 0 for a {len} byte config");
                }
                memory.write(&mut *store, ptr as u32 as usize, config)?;
                (ptr, len)
            };
            inst.get_typed_func::<(i32, i32), ()>(&mut *store, "init")?
                .call(&mut *store, (ptr, len))
        })
    }

    /// Call `on_timer()`. A plugin that did not declare the trigger is never called.
    pub fn on_timer(&self, state: &StateSnapshot) -> Result<Effects, CallError> {
        if !self.info.caps.triggers.on_timer {
            return Err(CallError::CapabilityDenied("on_timer"));
        }
        self.run(state, |store, inst| {
            inst.get_typed_func::<(), ()>(&mut *store, "on_timer")?
                .call(&mut *store, ())
        })
    }

    fn store(&self, snapshot: Arc<StateSnapshot>) -> Store<CallState> {
        let state_bytes = snapshot.iter().map(|(k, v)| k.len() + v.len()).sum();
        let state = CallState {
            limits: StoreLimitsBuilder::new()
                .memory_size(self.limits.max_memory_bytes)
                .instances(1)
                .memories(1)
                .table_elements(self.limits.max_table_elements)
                .tables(4)
                .trap_on_grow_failure(true)
                .build(),
            caps: self.info.caps.clone(),
            run: self.limits.clone(),
            snapshot,
            puts: BTreeMap::new(),
            state_bytes,
            state_ops: 0,
            logs: Vec::new(),
            logs_dropped: 0,
            denied: None,
        };
        let mut store = Store::new(&self.engine, state);
        store.limiter(|s| &mut s.limits);
        store.set_epoch_deadline(EPOCH_DEADLINE_TICKS);
        store
    }

    fn run(
        &self,
        state: &StateSnapshot,
        f: impl FnOnce(&mut Store<CallState>, &Instance) -> wasmtime::Result<()>,
    ) -> Result<Effects, CallError> {
        let mut store = self.store(Arc::new(state.clone()));
        let result = self
            .linker
            .instantiate(&mut store, &self.module)
            .and_then(|inst| f(&mut store, &inst));
        let st = store.into_data();
        if let Err(e) = result {
            if let Some(cap) = st.denied {
                return Err(CallError::CapabilityDenied(cap));
            }
            if e.downcast_ref::<Trap>() == Some(&Trap::Interrupt) {
                return Err(CallError::Timeout);
            }
            return Err(CallError::Trap(format!("{e:#}")));
        }
        Ok(Effects {
            state_puts: st.puts,
            logs: st.logs,
            logs_dropped: st.logs_dropped,
        })
    }
}

fn guest_memory(store: &mut Store<CallState>, inst: &Instance) -> wasmtime::Result<Memory> {
    inst.get_memory(&mut *store, "memory")
        .ok_or_else(|| wasmtime::format_err!("the module has no `memory` export"))
}

fn caller_memory(caller: &mut Caller<'_, CallState>) -> wasmtime::Result<Memory> {
    match caller.get_export("memory") {
        Some(Extern::Memory(m)) => Ok(m),
        _ => wasmtime::bail!("the module has no `memory` export"),
    }
}

/// Copy `len` bytes at `ptr` out of guest memory; out of bounds traps the call.
fn read_guest(caller: &mut Caller<'_, CallState>, ptr: i32, len: i32) -> wasmtime::Result<Vec<u8>> {
    let memory = caller_memory(caller)?;
    let (ptr, len) = (ptr as u32 as usize, len as u32 as usize);
    let end = ptr
        .checked_add(len)
        .ok_or_else(|| wasmtime::format_err!("pointer overflow"))?;
    memory
        .data(&*caller)
        .get(ptr..end)
        .map(<[u8]>::to_vec)
        .ok_or_else(|| wasmtime::format_err!("guest pointer out of bounds"))
}

fn deny(caller: &mut Caller<'_, CallState>, cap: &'static str) -> wasmtime::Error {
    caller.data_mut().denied = Some(cap);
    wasmtime::format_err!("capability `{cap}` was not approved")
}

fn linker(engine: &Engine) -> Linker<CallState> {
    let mut linker = Linker::new(engine);
    linker
        .func_wrap(
            "wayhouse",
            "log",
            |mut caller: Caller<'_, CallState>,
             level: i32,
             ptr: i32,
             len: i32|
             -> wasmtime::Result<()> {
                if !caller.data().caps.log {
                    return Err(deny(&mut caller, "log"));
                }
                let raw = read_guest(&mut caller, ptr, len)?;
                let st = caller.data_mut();
                if st.logs.len() >= st.run.max_log_lines {
                    st.logs_dropped += 1;
                    return Ok(());
                }
                let mut msg = String::from_utf8_lossy(&raw).into_owned();
                if msg.len() > MAX_LOG_LINE_BYTES {
                    let mut cut = MAX_LOG_LINE_BYTES;
                    while !msg.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    msg.truncate(cut);
                }
                let level = match level {
                    0 => LogLevel::Error,
                    1 => LogLevel::Warn,
                    2 => LogLevel::Info,
                    _ => LogLevel::Debug,
                };
                st.logs.push(LogLine { level, msg });
                Ok(())
            },
        )
        .expect("defining wayhouse.log");
    linker
        .func_wrap(
            "wayhouse",
            "state_get",
            |mut caller: Caller<'_, CallState>,
             kptr: i32,
             klen: i32,
             out_ptr: i32,
             out_cap: i32|
             -> wasmtime::Result<i32> {
                if caller.data().caps.state.is_none() {
                    return Err(deny(&mut caller, "state"));
                }
                let key = read_guest(&mut caller, kptr, klen)?;
                let Ok(key) = String::from_utf8(key) else {
                    return Ok(-1);
                };
                let Some(value) = caller.data().current(&key).cloned() else {
                    return Ok(-1);
                };
                let len = i32::try_from(value.len())?;
                if len <= out_cap {
                    let memory = caller_memory(&mut caller)?;
                    memory.write(&mut caller, out_ptr as u32 as usize, &value)?;
                }
                Ok(len)
            },
        )
        .expect("defining wayhouse.state_get");
    linker
        .func_wrap(
            "wayhouse",
            "state_put",
            |mut caller: Caller<'_, CallState>,
             kptr: i32,
             klen: i32,
             vptr: i32,
             vlen: i32|
             -> wasmtime::Result<i32> {
                let Some(cap) = caller.data().caps.state else {
                    return Err(deny(&mut caller, "state"));
                };
                let key = read_guest(&mut caller, kptr, klen)?;
                let value = read_guest(&mut caller, vptr, vlen)?;
                let Ok(key) = String::from_utf8(key) else {
                    return Ok(-1);
                };
                let st = caller.data_mut();
                if key.len() > MAX_KEY_BYTES || st.state_ops >= st.run.max_state_ops {
                    return Ok(-1);
                }
                let old = st.current(&key).map_or(0, |v| key.len() + v.len());
                let new_total = st.state_bytes - old + key.len() + value.len();
                if new_total > cap.max_bytes {
                    return Ok(-1);
                }
                st.state_ops += 1;
                st.state_bytes = new_total;
                st.puts.insert(key, value);
                Ok(0)
            },
        )
        .expect("defining wayhouse.state_put");
    linker
}
