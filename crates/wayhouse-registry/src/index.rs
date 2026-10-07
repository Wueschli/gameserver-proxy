//! The registry `index.json` format and every rule on it.

use serde::{Deserialize, Serialize};

/// The only index schema this crate understands.
pub const INDEX_SCHEMA: u32 = 1;
/// Largest index file accepted (1 MiB).
pub const MAX_INDEX_BYTES: usize = 1024 * 1024;
/// Largest module a registry may list. Equal to `MAX_MODULE_BYTES` in the proxy's
/// `sniffer_loader.rs` (the source of truth); keep the two in step.
pub const MAX_MODULE_BYTES: u64 = 8 * 1024 * 1024;

/// What a registry serves. Plugins are reserved for the plugin wave.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Sniffer,
    Plugin,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Index {
    pub schema: u32,
    pub kind: Kind,
    pub name: String,
    pub sniffers: Vec<SnifferEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnifferEntry {
    pub name: String,
    pub description: String,
    pub license: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    pub versions: Vec<VersionEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionEntry {
    pub version: semver::Version,
    pub abi: String,
    pub min_proxy: semver::Version,
    pub url: String,
    pub sha256: String,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature_url: Option<String>,
    pub limits: Limits,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    pub max_memory_bytes: u64,
    pub call_timeout_ms: u64,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum IndexError {
    #[error("index is {0} bytes, over the {MAX_INDEX_BYTES} byte limit")]
    TooLarge(usize),
    #[error("index is not valid JSON for this format: {0}")]
    Json(String),
    #[error("unsupported index schema {0} (this client understands schema {INDEX_SCHEMA})")]
    UnsupportedSchema(u32),
    #[error("registry kind {0:?} is not supported yet (only sniffer registries)")]
    UnsupportedKind(Kind),
    #[error("invalid sniffer name {0:?}: use only letters, digits, '-' and '_'")]
    BadName(String),
    #[error("sniffer {0:?} is listed twice")]
    DuplicateSniffer(String),
    #[error("sniffer {sniffer:?} lists version {version} twice")]
    DuplicateVersion { sniffer: String, version: String },
    #[error("sniffer {sniffer:?}: versions must be sorted newest first ({later} is listed after {earlier})")]
    UnsortedVersions {
        sniffer: String,
        earlier: String,
        later: String,
    },
    #[error("sniffer {sniffer:?} {version}: {field} must be an https:// URL, got {url:?}")]
    NotHttps {
        sniffer: String,
        version: String,
        field: &'static str,
        url: String,
    },
    #[error("sniffer {sniffer:?} {version}: sha256 must be 64 lowercase hex characters")]
    BadSha256 { sniffer: String, version: String },
    #[error("sniffer {sniffer:?} {version}: size {size} is over the {MAX_MODULE_BYTES} byte module limit")]
    OversizeModule {
        sniffer: String,
        version: String,
        size: u64,
    },
    #[error("sniffer {sniffer:?} {version}: abi {abi:?} is not of the form major.minor")]
    BadAbi {
        sniffer: String,
        version: String,
        abi: String,
    },
}

/// Just the `kind`, read before the rest of the index (see `parse_index`).
#[derive(Deserialize)]
struct Head {
    kind: Option<Kind>,
}

/// Parse and validate an index. Never panics on hostile input.
pub fn parse_index(bytes: &[u8]) -> Result<Index, IndexError> {
    if bytes.len() > MAX_INDEX_BYTES {
        return Err(IndexError::TooLarge(bytes.len()));
    }
    // Look at `kind` first: another kind of registry has a different payload, and
    // "missing field `sniffers`" would not tell the operator what they added.
    if let Ok(Head {
        kind: Some(kind @ Kind::Plugin),
    }) = serde_json::from_slice(bytes)
    {
        return Err(IndexError::UnsupportedKind(kind));
    }
    let index: Index =
        serde_json::from_slice(bytes).map_err(|e| IndexError::Json(e.to_string()))?;
    index.validate()?;
    Ok(index)
}

impl Index {
    pub fn validate(&self) -> Result<(), IndexError> {
        if self.schema != INDEX_SCHEMA {
            return Err(IndexError::UnsupportedSchema(self.schema));
        }
        if self.kind != Kind::Sniffer {
            return Err(IndexError::UnsupportedKind(self.kind));
        }
        let mut seen = std::collections::HashSet::new();
        for sniffer in &self.sniffers {
            if !valid_name(&sniffer.name) {
                return Err(IndexError::BadName(sniffer.name.clone()));
            }
            if !seen.insert(sniffer.name.as_str()) {
                return Err(IndexError::DuplicateSniffer(sniffer.name.clone()));
            }
            sniffer.validate()?;
        }
        Ok(())
    }
}

impl SnifferEntry {
    fn validate(&self) -> Result<(), IndexError> {
        for pair in self.versions.windows(2) {
            let (earlier, later) = (&pair[0].version, &pair[1].version);
            if earlier == later {
                return Err(IndexError::DuplicateVersion {
                    sniffer: self.name.clone(),
                    version: earlier.to_string(),
                });
            }
            if earlier < later {
                return Err(IndexError::UnsortedVersions {
                    sniffer: self.name.clone(),
                    earlier: earlier.to_string(),
                    later: later.to_string(),
                });
            }
        }
        for v in &self.versions {
            v.validate(&self.name)?;
        }
        Ok(())
    }
}

impl VersionEntry {
    fn validate(&self, sniffer: &str) -> Result<(), IndexError> {
        let version = self.version.to_string();
        let https = |field: &'static str, url: &str| {
            if url.starts_with("https://") {
                Ok(())
            } else {
                Err(IndexError::NotHttps {
                    sniffer: sniffer.to_owned(),
                    version: version.clone(),
                    field,
                    url: url.to_owned(),
                })
            }
        };
        https("url", &self.url)?;
        if let Some(sig) = &self.signature_url {
            https("signature_url", sig)?;
        }
        if self.sha256.len() != 64
            || !self
                .sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(IndexError::BadSha256 {
                sniffer: sniffer.to_owned(),
                version,
            });
        }
        if self.size > MAX_MODULE_BYTES {
            return Err(IndexError::OversizeModule {
                sniffer: sniffer.to_owned(),
                version,
                size: self.size,
            });
        }
        if parse_abi(&self.abi).is_none() {
            return Err(IndexError::BadAbi {
                sniffer: sniffer.to_owned(),
                version,
                abi: self.abi.clone(),
            });
        }
        Ok(())
    }
}

/// Same rule as `valid_module_name` in the proxy's `admin.rs`.
pub(crate) fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Parse an ABI string `major.minor` (both `u16`).
pub(crate) fn parse_abi(abi: &str) -> Option<(u16, u16)> {
    let (major, minor) = abi.split_once('.')?;
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(major) || !digits(minor) {
        return None;
    }
    Some((major.parse().ok()?, minor.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = include_str!("../tests/fixtures/example-index.json");

    fn example() -> serde_json::Value {
        serde_json::from_str(EXAMPLE).unwrap()
    }

    fn parse(v: &serde_json::Value) -> Result<Index, IndexError> {
        parse_index(v.to_string().as_bytes())
    }

    fn first_version(v: &mut serde_json::Value) -> &mut serde_json::Value {
        &mut v["sniffers"][0]["versions"][0]
    }

    #[test]
    fn parses_the_example_from_the_spec() {
        let index = parse_index(EXAMPLE.as_bytes()).unwrap();
        assert_eq!(index.kind, Kind::Sniffer);
        assert_eq!(index.sniffers[0].versions.len(), 2);
        assert_eq!(index.sniffers[0].versions[0].version.to_string(), "0.2.0");
    }

    #[test]
    fn rejects_schema_2() {
        let mut v = example();
        v["schema"] = 2.into();
        assert_eq!(parse(&v), Err(IndexError::UnsupportedSchema(2)));
    }

    #[test]
    fn a_plugin_registry_gets_the_kind_error_not_a_missing_field() {
        // A plugin index lists `plugins`, not `sniffers`; the message must say what
        // the registry is, not that a field is missing.
        let plugin_index = br#"{"schema":1,"kind":"plugin","name":"x","plugins":[]}"#;
        assert_eq!(
            parse_index(plugin_index),
            Err(IndexError::UnsupportedKind(Kind::Plugin))
        );
    }

    #[test]
    fn rejects_missing_kind() {
        let mut v = example();
        v.as_object_mut().unwrap().remove("kind");
        assert!(matches!(parse(&v), Err(IndexError::Json(m)) if m.contains("kind")));
    }

    #[test]
    fn rejects_plugin_kind_until_supported() {
        let mut v = example();
        v["kind"] = "plugin".into();
        assert_eq!(parse(&v), Err(IndexError::UnsupportedKind(Kind::Plugin)));
    }

    #[test]
    fn rejects_duplicate_sniffer_name() {
        let mut v = example();
        let dup = v["sniffers"][0].clone();
        v["sniffers"].as_array_mut().unwrap().push(dup);
        assert_eq!(
            parse(&v),
            Err(IndexError::DuplicateSniffer("minecraft".into()))
        );
    }

    #[test]
    fn rejects_duplicate_version() {
        let mut v = example();
        v["sniffers"][0]["versions"][1]["version"] = "0.2.0".into();
        assert!(matches!(
            parse(&v),
            Err(IndexError::DuplicateVersion { .. })
        ));
    }

    #[test]
    fn rejects_unsorted_versions() {
        let mut v = example();
        v["sniffers"][0]["versions"]
            .as_array_mut()
            .unwrap()
            .reverse();
        assert!(matches!(
            parse(&v),
            Err(IndexError::UnsortedVersions { .. })
        ));
    }

    #[test]
    fn rejects_http_url() {
        let mut v = example();
        first_version(&mut v)["url"] = "http://example.com/a.wasm".into();
        assert!(matches!(
            parse(&v),
            Err(IndexError::NotHttps { field: "url", .. })
        ));
    }

    #[test]
    fn rejects_http_signature_url() {
        let mut v = example();
        first_version(&mut v)["signature_url"] = "http://example.com/a.minisig".into();
        assert!(matches!(
            parse(&v),
            Err(IndexError::NotHttps {
                field: "signature_url",
                ..
            })
        ));
    }

    #[test]
    fn rejects_uppercase_or_short_sha() {
        for bad in [
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "abc",
        ] {
            let mut v = example();
            first_version(&mut v)["sha256"] = bad.into();
            assert!(
                matches!(parse(&v), Err(IndexError::BadSha256 { .. })),
                "{bad}"
            );
        }
    }

    #[test]
    fn rejects_bad_name() {
        for bad in ["../evil", "", "has space", ".", ".."] {
            let mut v = example();
            v["sniffers"][0]["name"] = bad.into();
            assert!(matches!(parse(&v), Err(IndexError::BadName(_))), "{bad:?}");
        }
    }

    #[test]
    fn rejects_oversize_module() {
        let mut v = example();
        first_version(&mut v)["size"] = (MAX_MODULE_BYTES + 1).into();
        assert!(matches!(parse(&v), Err(IndexError::OversizeModule { .. })));
    }

    #[test]
    fn accepts_a_module_of_exactly_the_limit() {
        let mut v = example();
        first_version(&mut v)["size"] = MAX_MODULE_BYTES.into();
        assert!(parse(&v).is_ok());
    }

    #[test]
    fn rejects_bad_abi_string() {
        for bad in ["1", "a.b", "1.2.3", ""] {
            let mut v = example();
            first_version(&mut v)["abi"] = bad.into();
            assert!(
                matches!(parse(&v), Err(IndexError::BadAbi { .. })),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn rejects_oversize_index_bytes() {
        let big = vec![b' '; MAX_INDEX_BYTES + 1];
        assert_eq!(
            parse_index(&big),
            Err(IndexError::TooLarge(MAX_INDEX_BYTES + 1))
        );
    }

    #[test]
    fn rejects_garbage_json_without_panic() {
        for garbage in [
            &b""[..],
            b"{",
            b"null",
            b"[]",
            b"\xff\xfe",
            b"{\"schema\":1}",
        ] {
            assert!(matches!(parse_index(garbage), Err(IndexError::Json(_))));
        }
    }

    #[test]
    fn unknown_fields_are_ignored_for_forward_compatibility() {
        let mut v = example();
        v["future_field"] = true.into();
        first_version(&mut v)["future"] = 1.into();
        assert!(parse(&v).is_ok());
    }
}
