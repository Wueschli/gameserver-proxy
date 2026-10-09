//! Loads and runs wayhouse WASM plugins (Wave 5). See `docs/plugins.md`.

pub mod bounds;
pub mod caps;
pub mod conformance;
pub mod http;
pub mod module;
pub mod pool;
pub mod runtime;
pub mod secret;

pub use bounds::Bounds;
pub use caps::{Capabilities, HttpCap, HttpHost, SecretSlot, StateCap, Triggers, MAX_STATE_BYTES};
pub use http::{HttpEngine, Transport};
pub use module::{inspect, AbiVersion, ModuleError, ModuleInfo, HOST_ABI, MAX_MODULE_BYTES};
pub use pool::{CompilePool, PoolError};
pub use runtime::{
    CallError, Effects, Limits, LogLevel, LogLine, Plugin, PluginHost, StateSnapshot, MAX_KEY_BYTES,
};
pub use secret::{SecretSource, SecretValue};
