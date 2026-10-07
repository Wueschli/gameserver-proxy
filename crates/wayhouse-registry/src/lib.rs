//! Sniffer registry formats and checks. See the crate's `Cargo.toml` for scope.

pub mod index;

pub use index::{
    parse_index, Index, IndexError, Kind, Limits, SnifferEntry, VersionEntry, INDEX_SCHEMA,
    MAX_INDEX_BYTES, MAX_MODULE_BYTES,
};
