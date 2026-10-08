//! Loads and runs wayhouse WASM plugins (Wave 5). See `docs/plugins.md`.

pub mod caps;
pub mod module;
pub mod runtime;

pub use caps::{Capabilities, StateCap, Triggers};
pub use module::{inspect, AbiVersion, ModuleError, ModuleInfo, HOST_ABI, MAX_MODULE_BYTES};
pub use runtime::{
    CallError, Effects, Limits, LogLevel, LogLine, Plugin, PluginHost, StateSnapshot,
};
