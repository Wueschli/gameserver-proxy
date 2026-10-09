//! Loads and runs wayhouse WASM plugins (Wave 5). See `docs/plugins.md`.

pub mod bounds;
pub mod caps;
pub mod conformance;
pub mod module;
pub mod pool;
pub mod runtime;

pub use bounds::Bounds;
pub use caps::{Capabilities, StateCap, Triggers, MAX_STATE_BYTES};
pub use module::{inspect, AbiVersion, ModuleError, ModuleInfo, HOST_ABI, MAX_MODULE_BYTES};
pub use pool::{CompilePool, PoolError};
pub use runtime::{
    CallError, Effects, Limits, LogLevel, LogLine, Plugin, PluginHost, StateSnapshot, MAX_KEY_BYTES,
};
