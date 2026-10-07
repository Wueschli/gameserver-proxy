//! The per-sniffer source manifest (`sniffers/<name>/manifest.toml` in the sniffers repo).

use crate::index::{parse_abi, Limits};
use serde::Deserialize;

/// What a sniffer author writes. Strict (`deny_unknown_fields`): a misspelt key in
/// source the author controls is an error, unlike the forward-compatible index.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub name: String,
    pub description: String,
    pub license: String,
    pub version: semver::Version,
    pub abi: String,
    pub min_proxy: semver::Version,
    pub limits: Limits,
    #[serde(default)]
    pub config: Option<String>,
    #[serde(default)]
    pub homepage: Option<String>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ManifestError {
    #[error("manifest.toml: {0}")]
    Toml(String),
    #[error("manifest.toml: abi {0:?} is not of the form major.minor")]
    BadAbi(String),
}

pub fn parse_manifest(text: &str) -> Result<Manifest, ManifestError> {
    let m: Manifest = toml::from_str(text).map_err(|e| ManifestError::Toml(e.to_string()))?;
    if parse_abi(&m.abi).is_none() {
        return Err(ManifestError::BadAbi(m.abi));
    }
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) const MANIFEST: &str = r#"
name = "minecraft"
description = "Minecraft handshake virtual hosts"
license = "MIT OR Apache-2.0"
version = "0.1.0"
abi = "0.1"
min_proxy = "0.1.0"
homepage = "https://github.com/wayhouse-proxy/sniffers/tree/main/sniffers/minecraft"
config = "optional documentation"

[limits]
max_memory_bytes = 1048576
call_timeout_ms = 50
"#;

    #[test]
    fn manifest_round_trip() {
        let m = parse_manifest(MANIFEST).unwrap();
        assert_eq!(m.name, "minecraft");
        assert_eq!(m.version.to_string(), "0.1.0");
        assert_eq!(m.limits.call_timeout_ms, 50);
        assert_eq!(m.config.as_deref(), Some("optional documentation"));
    }

    #[test]
    fn manifest_rejects_unknown_abi_format() {
        let text = MANIFEST.replace("abi = \"0.1\"", "abi = \"v1\"");
        assert_eq!(
            parse_manifest(&text),
            Err(ManifestError::BadAbi("v1".into()))
        );
    }

    #[test]
    fn manifest_rejects_a_misspelt_key() {
        let text = MANIFEST.replace("min_proxy", "min_proxie");
        assert!(matches!(parse_manifest(&text), Err(ManifestError::Toml(_))));
    }

    #[test]
    fn manifest_rejects_missing_required_field() {
        let text = MANIFEST.replace("license = \"MIT OR Apache-2.0\"\n", "");
        assert!(
            matches!(parse_manifest(&text), Err(ManifestError::Toml(m)) if m.contains("license"))
        );
    }

    #[test]
    fn manifest_rejects_a_non_semver_version() {
        let text = MANIFEST.replace("version = \"0.1.0\"", "version = \"1\"");
        assert!(matches!(parse_manifest(&text), Err(ManifestError::Toml(_))));
    }
}
