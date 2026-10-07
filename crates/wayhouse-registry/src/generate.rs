//! Deterministic `index.json` generation from manifests and built artifacts.

use crate::index::{Index, IndexError, Kind, SnifferEntry, VersionEntry, INDEX_SCHEMA};
use crate::manifest::Manifest;
use crate::verify::check_module;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// One built sniffer version to publish.
#[derive(Debug, Clone)]
pub struct Artifact {
    pub manifest: Manifest,
    pub bytes: Vec<u8>,
    pub url: String,
    pub signature_url: Option<String>,
}

/// The new index and the versions that were not in `previous`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Generated {
    pub index: Index,
    pub added: Vec<(String, semver::Version)>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum GenerateError {
    #[error("{name} {version}: {reason}")]
    Module {
        name: String,
        version: String,
        reason: String,
    },
    #[error("{name} {version}: the built module declares ABI {declared} but manifest.toml says {manifest}")]
    AbiMismatch {
        name: String,
        version: String,
        declared: String,
        manifest: String,
    },
    #[error("{name} {version} is already published with sha256 {published}; a published version is immutable (new build is {built}); bump the version")]
    Republished {
        name: String,
        version: String,
        published: String,
        built: String,
    },
    #[error("the generated index is invalid: {0}")]
    Invalid(#[from] IndexError),
}

/// Merge `artifacts` into `previous` (older versions are kept) and return the new,
/// validated index with sniffers sorted by name and versions newest first.
pub fn generate(
    registry_name: &str,
    artifacts: &[Artifact],
    previous: Option<&Index>,
) -> Result<Generated, GenerateError> {
    let mut sniffers: BTreeMap<String, SnifferEntry> = previous
        .map(|p| {
            p.sniffers
                .iter()
                .map(|s| (s.name.clone(), s.clone()))
                .collect()
        })
        .unwrap_or_default();
    // The artifact that supplies each touched sniffer's entry text (description,
    // license, homepage): the newest version published in this run.
    let mut text_from: BTreeMap<&str, &Artifact> = BTreeMap::new();
    let mut added = Vec::new();

    for a in artifacts {
        let m = &a.manifest;
        let version = m.version.to_string();
        let declared = check_module(&a.bytes).map_err(|e| GenerateError::Module {
            name: m.name.clone(),
            version: version.clone(),
            reason: e.to_string(),
        })?;
        let declared = format!("{}.{}", declared.major, declared.minor);
        if declared != m.abi {
            return Err(GenerateError::AbiMismatch {
                name: m.name.clone(),
                version,
                declared,
                manifest: m.abi.clone(),
            });
        }
        let sha256 = Sha256::digest(&a.bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let entry = sniffers
            .entry(m.name.clone())
            .or_insert_with(|| SnifferEntry {
                name: m.name.clone(),
                description: m.description.clone(),
                license: m.license.clone(),
                homepage: m.homepage.clone(),
                versions: Vec::new(),
            });
        match entry.versions.iter().find(|v| v.version == m.version) {
            Some(published) if published.sha256 == sha256 => {}
            Some(published) => {
                return Err(GenerateError::Republished {
                    name: m.name.clone(),
                    version,
                    published: published.sha256.clone(),
                    built: sha256,
                });
            }
            None => {
                entry.versions.push(VersionEntry {
                    version: m.version.clone(),
                    abi: m.abi.clone(),
                    min_proxy: m.min_proxy.clone(),
                    url: a.url.clone(),
                    sha256,
                    size: a.bytes.len() as u64,
                    signature_url: a.signature_url.clone(),
                    limits: m.limits.clone(),
                    config: m.config.clone(),
                });
                added.push((m.name.clone(), m.version.clone()));
            }
        }
        match text_from.get(m.name.as_str()) {
            Some(prev) if prev.manifest.version >= m.version => {}
            _ => {
                text_from.insert(&m.name, a);
            }
        }
    }

    for entry in sniffers.values_mut() {
        entry.versions.sort_by(|a, b| b.version.cmp(&a.version));
        if let Some(a) = text_from.get(entry.name.as_str()) {
            // Only the newest version of the sniffer speaks for it.
            if entry
                .versions
                .first()
                .is_some_and(|v| v.version == a.manifest.version)
            {
                entry.description = a.manifest.description.clone();
                entry.license = a.manifest.license.clone();
                entry.homepage = a.manifest.homepage.clone();
            }
        }
    }
    added.sort();

    let index = Index {
        schema: INDEX_SCHEMA,
        kind: Kind::Sniffer,
        name: registry_name.to_owned(),
        sniffers: sniffers.into_values().collect(),
    };
    index.validate()?;
    Ok(Generated { index, added })
}

/// The canonical serialisation: pretty JSON, fixed field order, trailing newline.
pub fn to_json(index: &Index) -> String {
    let mut s = serde_json::to_string_pretty(index).expect("an Index always serialises");
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::parse_manifest;

    fn wasm(abi_minor: u8, marker: &str) -> Vec<u8> {
        wat::parse_str(format!(
            r#"(module (memory 1) (func (export "{marker}")) (@custom "wayhouse.abi" "\00\00\0{abi_minor}\00"))"#
        ))
        .unwrap()
    }

    fn manifest(name: &str, version: &str, abi: &str) -> Manifest {
        parse_manifest(&format!(
            r#"name = "{name}"
description = "{name} sniffer"
license = "MIT"
version = "{version}"
abi = "{abi}"
min_proxy = "0.1.0"
[limits]
max_memory_bytes = 1048576
call_timeout_ms = 50
"#
        ))
        .unwrap()
    }

    fn artifact(name: &str, version: &str, marker: &str) -> Artifact {
        Artifact {
            manifest: manifest(name, version, "0.1"),
            bytes: wasm(1, marker),
            url: format!("https://example.com/{name}-v{version}/{name}.wasm"),
            signature_url: None,
        }
    }

    #[test]
    fn generate_computes_sha_and_size() {
        let a = artifact("a2s", "0.1.0", "x");
        let g = generate("test", std::slice::from_ref(&a), None).unwrap();
        let v = &g.index.sniffers[0].versions[0];
        assert_eq!(v.size, a.bytes.len() as u64);
        let want: String = Sha256::digest(&a.bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(v.sha256, want);
        assert_eq!(g.added, vec![("a2s".to_string(), "0.1.0".parse().unwrap())]);
        assert_eq!(g.index.kind, Kind::Sniffer);
        assert_eq!(g.index.schema, INDEX_SCHEMA);
    }

    #[test]
    fn generate_rejects_abi_mismatch_between_manifest_and_module() {
        let mut a = artifact("a2s", "0.1.0", "x");
        a.manifest.abi = "0.2".into();
        assert!(matches!(
            generate("test", &[a], None),
            Err(GenerateError::AbiMismatch { .. })
        ));
    }

    #[test]
    fn generate_rejects_a_module_that_is_not_a_sniffer() {
        let mut a = artifact("a2s", "0.1.0", "x");
        a.bytes = b"not wasm".to_vec();
        assert!(matches!(
            generate("test", &[a], None),
            Err(GenerateError::Module { .. })
        ));
    }

    #[test]
    fn generate_keeps_previous_versions() {
        let first = generate("test", &[artifact("a2s", "0.1.0", "x")], None)
            .unwrap()
            .index;
        let second = generate("test", &[artifact("a2s", "0.2.0", "y")], Some(&first)).unwrap();
        let versions: Vec<_> = second.index.sniffers[0]
            .versions
            .iter()
            .map(|v| v.version.to_string())
            .collect();
        assert_eq!(versions, ["0.2.0", "0.1.0"]);
        assert_eq!(second.added.len(), 1);
    }

    #[test]
    fn generate_republishing_identical_bytes_is_a_no_op() {
        let a = artifact("a2s", "0.1.0", "x");
        let first = generate("test", std::slice::from_ref(&a), None)
            .unwrap()
            .index;
        let again = generate("test", &[a], Some(&first)).unwrap();
        assert_eq!(again.index, first);
        assert!(again.added.is_empty());
    }

    #[test]
    fn generate_rejects_a_version_already_published_with_a_different_sha() {
        let first = generate("test", &[artifact("a2s", "0.1.0", "x")], None)
            .unwrap()
            .index;
        let r = generate(
            "test",
            &[artifact("a2s", "0.1.0", "different")],
            Some(&first),
        );
        assert!(matches!(r, Err(GenerateError::Republished { .. })), "{r:?}");
    }

    #[test]
    fn generate_sorts_sniffers_and_versions() {
        let g = generate(
            "test",
            &[
                artifact("zeta", "0.1.0", "a"),
                artifact("alpha", "0.1.0", "b"),
                artifact("alpha", "0.3.0", "c"),
                artifact("alpha", "0.2.0", "d"),
            ],
            None,
        )
        .unwrap();
        let names: Vec<_> = g.index.sniffers.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["alpha", "zeta"]);
        let versions: Vec<_> = g.index.sniffers[0]
            .versions
            .iter()
            .map(|v| v.version.to_string())
            .collect();
        assert_eq!(versions, ["0.3.0", "0.2.0", "0.1.0"]);
    }

    #[test]
    fn generate_takes_entry_text_from_the_newest_version() {
        let mut old = artifact("a2s", "0.1.0", "x");
        old.manifest.description = "old text".into();
        let mut new = artifact("a2s", "0.2.0", "y");
        new.manifest.description = "new text".into();
        let g = generate("test", &[new, old], None).unwrap();
        assert_eq!(g.index.sniffers[0].description, "new text");
    }

    #[test]
    fn generate_is_deterministic() {
        let arts = [
            artifact("zeta", "0.1.0", "a"),
            artifact("alpha", "0.1.0", "b"),
            artifact("alpha", "0.2.0", "c"),
        ];
        let one = to_json(&generate("test", &arts, None).unwrap().index);
        let mut shuffled = arts.to_vec();
        shuffled.reverse();
        let two = to_json(&generate("test", &shuffled, None).unwrap().index);
        assert_eq!(one, two);
        assert!(one.ends_with("}\n"));
    }

    #[test]
    fn the_generated_index_round_trips_through_the_parser() {
        let g = generate("test", &[artifact("a2s", "0.1.0", "x")], None).unwrap();
        assert_eq!(
            crate::parse_index(to_json(&g.index).as_bytes()).unwrap(),
            g.index
        );
    }
}
