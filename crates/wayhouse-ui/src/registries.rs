//! The operator's list of sniffer registries, persisted as a JSON file.
//!
//! One registry is the official one (a constant, removable); the rest are
//! external and installed from at the operator's own risk. Nothing here touches
//! the network: [`crate::registry_client`] fetches, this module only remembers
//! which URLs to fetch.

use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Where the official registry's `index.json` lives. One constant so a later
/// move of the index is a one-line change.
pub const DEFAULT_REGISTRY_URL: &str =
    "https://raw.githubusercontent.com/wayhouse-proxy/sniffers/main/index.json";

/// Most registries kept at once.
pub const MAX_REGISTRIES: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryRef {
    /// First 12 hex characters of the sha256 of the normalised URL.
    pub id: String,
    pub name: String,
    pub url: String,
    pub official: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum RegistriesError {
    #[error("registry URL must be a valid https:// URL with a host")]
    BadUrl,
    #[error("registry URL must not contain credentials")]
    Credentials,
    #[error("at most {MAX_REGISTRIES} registries")]
    TooMany,
    #[error("registries file: {0}")]
    File(String),
}

/// What the file holds: only what the operator changed. The official registry is
/// a constant, so it is never written; removing it is remembered as a flag.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Stored {
    #[serde(default)]
    registries: Vec<RegistryRef>,
    #[serde(default)]
    default_removed: bool,
}

struct Inner {
    added: Vec<RegistryRef>,
    default_removed: bool,
}

pub struct Registries {
    path: Option<PathBuf>,
    include_default: bool,
    inner: Mutex<Inner>,
}

fn id_for(url: &str) -> String {
    let digest = Sha256::digest(url.as_bytes());
    digest.iter().take(6).map(|b| format!("{b:02x}")).collect()
}

fn default_ref() -> RegistryRef {
    RegistryRef {
        id: id_for(DEFAULT_REGISTRY_URL),
        name: "Official wayhouse sniffers".into(),
        url: DEFAULT_REGISTRY_URL.into(),
        official: true,
    }
}

/// The normalised form of a registry URL, or why it is refused.
fn normalise(url: &str) -> Result<String, RegistriesError> {
    let parsed = reqwest::Url::parse(url).map_err(|_| RegistriesError::BadUrl)?;
    if parsed.scheme() != "https" || parsed.host_str().is_none_or(str::is_empty) {
        return Err(RegistriesError::BadUrl);
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(RegistriesError::Credentials);
    }
    Ok(parsed.to_string())
}

impl Registries {
    pub fn load(path: Option<PathBuf>, include_default: bool) -> Result<Self, RegistriesError> {
        let stored = match &path {
            Some(p) if p.exists() => {
                let bytes = std::fs::read(p).map_err(|e| RegistriesError::File(e.to_string()))?;
                serde_json::from_slice::<Stored>(&bytes)
                    .map_err(|e| RegistriesError::File(format!("{}: {e}", p.display())))?
            }
            _ => Stored::default(),
        };
        Ok(Self {
            path,
            include_default,
            inner: Mutex::new(Inner {
                added: stored.registries,
                default_removed: stored.default_removed,
            }),
        })
    }

    fn snapshot(&self, inner: &Inner) -> Vec<RegistryRef> {
        let mut all = Vec::new();
        if self.include_default && !inner.default_removed {
            all.push(default_ref());
        }
        all.extend(inner.added.iter().cloned());
        all
    }

    pub fn list(&self) -> Vec<RegistryRef> {
        let inner = self.inner.lock().unwrap();
        self.snapshot(&inner)
    }

    pub fn get(&self, id: &str) -> Option<RegistryRef> {
        self.list().into_iter().find(|r| r.id == id)
    }

    pub fn add(&self, url: &str, name: Option<&str>) -> Result<RegistryRef, RegistriesError> {
        let url = normalise(url)?;
        let id = id_for(&url);
        let mut inner = self.inner.lock().unwrap();
        if let Some(existing) = self.snapshot(&inner).into_iter().find(|r| r.id == id) {
            return Ok(existing);
        }
        if self.snapshot(&inner).len() >= MAX_REGISTRIES {
            return Err(RegistriesError::TooMany);
        }
        let host = reqwest::Url::parse(&url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned))
            .unwrap_or_default();
        let entry = RegistryRef {
            id,
            name: name
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .map_or(host, |n| n.chars().take(80).collect()),
            url,
            official: false,
        };
        inner.added.push(entry.clone());
        self.persist(&inner)?;
        Ok(entry)
    }

    pub fn remove(&self, id: &str) -> Result<bool, RegistriesError> {
        let mut inner = self.inner.lock().unwrap();
        let before = inner.added.len();
        inner.added.retain(|r| r.id != id);
        let removed_default =
            self.include_default && !inner.default_removed && id == default_ref().id;
        if removed_default {
            inner.default_removed = true;
        }
        let changed = inner.added.len() != before || removed_default;
        if changed {
            self.persist(&inner)?;
        }
        Ok(changed)
    }

    /// Write the file atomically (temp file in the same directory, then rename),
    /// mode 0600. In-memory only when no path was configured.
    fn persist(&self, inner: &Inner) -> Result<(), RegistriesError> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let stored = Stored {
            registries: inner.added.clone(),
            default_removed: inner.default_removed,
        };
        let json =
            serde_json::to_vec_pretty(&stored).map_err(|e| RegistriesError::File(e.to_string()))?;
        let tmp = path.with_extension("json.tmp");
        write_private(&tmp, &json).map_err(|e| RegistriesError::File(e.to_string()))?;
        std::fs::rename(&tmp, path).map_err(|e| RegistriesError::File(e.to_string()))
    }
}

fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> Registries {
        Registries::load(None, true).unwrap()
    }

    #[test]
    fn default_registry_is_listed_and_official() {
        let l = fresh().list();
        assert_eq!(l.len(), 1);
        assert!(l[0].official);
        assert_eq!(l[0].url, DEFAULT_REGISTRY_URL);
        assert_eq!(l[0].id.len(), 12);
    }

    #[test]
    fn no_default_flag_hides_it() {
        assert!(Registries::load(None, false).unwrap().list().is_empty());
    }

    #[test]
    fn add_rejects_http() {
        let r = fresh().add("http://example.com/index.json", None);
        assert!(matches!(r, Err(RegistriesError::BadUrl)));
    }

    #[test]
    fn add_rejects_garbage_and_hostless_urls() {
        for u in ["not a url", "https://", "ftp://example.com/x", ""] {
            assert!(
                matches!(fresh().add(u, None), Err(RegistriesError::BadUrl)),
                "{u:?}"
            );
        }
    }

    #[test]
    fn add_rejects_userinfo_url() {
        let r = fresh().add("https://user:pw@example.com/index.json", None);
        assert!(matches!(r, Err(RegistriesError::Credentials)));
    }

    #[test]
    fn add_is_idempotent_same_id() {
        let r = fresh();
        let a = r
            .add("https://example.com/index.json", Some("mine"))
            .unwrap();
        let b = r
            .add("HTTPS://Example.com/index.json", Some("other"))
            .unwrap();
        assert_eq!(a.id, b.id);
        assert!(!a.official);
        assert_eq!(r.list().len(), 2, "default plus one");
    }

    #[test]
    fn add_caps_at_32() {
        let r = Registries::load(None, false).unwrap();
        for i in 0..MAX_REGISTRIES {
            r.add(&format!("https://example.com/{i}.json"), None)
                .unwrap();
        }
        let over = r.add("https://example.com/over.json", None);
        assert!(matches!(over, Err(RegistriesError::TooMany)));
        // Re-adding an existing one at the cap is still a no-op, not an error.
        r.add("https://example.com/0.json", None).unwrap();
    }

    #[test]
    fn remove_unknown_is_false_and_known_is_true() {
        let r = fresh();
        assert!(!r.remove("nope").unwrap());
        let id = r.list()[0].id.clone();
        assert!(r.remove(&id).unwrap());
        assert!(r.get(&id).is_none());
    }

    #[test]
    fn persists_atomically_and_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("registries.json");
        let r = Registries::load(Some(file.clone()), false).unwrap();
        let added = r
            .add("https://example.com/index.json", Some("mine"))
            .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let again = Registries::load(Some(file), false).unwrap();
        assert_eq!(again.list(), vec![added]);
    }

    #[test]
    fn default_is_not_persisted_but_removal_is_remembered() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("registries.json");
        let r = Registries::load(Some(file.clone()), true).unwrap();
        let id = r.list()[0].id.clone();
        r.remove(&id).unwrap();
        let again = Registries::load(Some(file), true).unwrap();
        assert!(again.get(&id).is_none(), "a removed default stays removed");
    }

    #[test]
    fn corrupt_file_is_an_error_not_silent_reset() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("registries.json");
        std::fs::write(&file, "{ not json").unwrap();
        assert!(matches!(
            Registries::load(Some(file), true),
            Err(RegistriesError::File(_))
        ));
    }
}
