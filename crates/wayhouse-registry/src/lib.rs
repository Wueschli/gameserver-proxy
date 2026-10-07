//! Sniffer registry formats and checks. See the crate's `Cargo.toml` for scope.

pub mod compat;
pub mod generate;
pub mod index;
pub mod manifest;
pub mod verify;

pub use compat::{abi_matches, select, AbiParseError, Environment, Incompatible};
pub use generate::{generate, to_json, Artifact, GenerateError, Generated};
pub use index::{
    parse_index, Index, IndexError, Kind, Limits, SnifferEntry, VersionEntry, INDEX_SCHEMA,
    MAX_INDEX_BYTES, MAX_MODULE_BYTES,
};
pub use manifest::{parse_manifest, Manifest, ManifestError};
pub use verify::{check_module, verify_artifact, AbiDecl, Verified, VerifyError};
