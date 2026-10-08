//! Plugin installs and module blobs (Wave 5; design:
//! `docs/superpowers/specs/2026-10-07-plugin-system-design.md`).
//!
//! A standalone controller keeps two sibling `sled` trees next to `revisions`:
//! `plugin_installs` (install id to [`InstallRecord`], JSON) and `plugin_blobs` (module
//! sha256 to the module bytes). A blob is content-addressed and shared by every install of
//! the same module; it is removed with the last install that references it. This slice
//! does not replicate anything: HA and slave controllers do not serve the plugin API.

pub mod api;

use serde::{Deserialize, Serialize};
use wayhouse_plugin_host::Capabilities;

/// Whether this controller serves the plugin API: `Err` carries the reason it does not.
/// Plugins are opt-in (`--plugins`) and, until the replicated slice, standalone-only.
pub fn availability(enabled: bool, ha: bool, slave: bool) -> Result<(), &'static str> {
    if !enabled {
        Err("plugins are off; start the controller with --plugins")
    } else if ha {
        Err("plugins are not supported with --ha-peers yet")
    } else if slave {
        Err("plugins run on the root tier only, not with --role slave")
    } else {
        Ok(())
    }
}

/// What the operator approved for one installed module.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallRecord {
    /// Random install id; route ownership (`plugin:<id>`) will use it, not the name.
    pub id: String,
    pub name: String,
    pub sha256: String,
    pub size: usize,
    /// The capability set approved against `sha256`.
    pub approved: Capabilities,
    /// Non-secret settings handed to `init`.
    pub config: serde_json::Value,
    pub enabled: bool,
    pub created_at: u64,
    pub created_by: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum PluginStoreError {
    #[error("plugin store: {0}")]
    Sled(#[from] sled::Error),
    #[error("plugin store record is corrupt: {0}")]
    Corrupt(#[from] serde_json::Error),
    #[error("the module for this install is no longer stored")]
    BlobMissing,
}

#[derive(Clone)]
pub struct PluginStore {
    installs: sled::Tree,
    blobs: sled::Tree,
    /// Serialises create, set_enabled and delete, so a blob cannot be
    /// garbage-collected between an install's blob check and its write.
    write: std::sync::Arc<std::sync::Mutex<()>>,
}

impl PluginStore {
    /// Open the two trees in the controller's database.
    pub fn open(db: &sled::Db) -> Result<Self, sled::Error> {
        Ok(Self {
            installs: db.open_tree("plugin_installs")?,
            blobs: db.open_tree("plugin_blobs")?,
            write: std::sync::Arc::default(),
        })
    }

    /// Keep `bytes` under its `sha256` (idempotent).
    pub fn put_blob(&self, sha256: &str, bytes: &[u8]) -> Result<(), PluginStoreError> {
        self.blobs.insert(sha256.as_bytes(), bytes)?;
        self.blobs.flush()?;
        Ok(())
    }

    pub fn get_blob(&self, sha256: &str) -> Result<Option<Vec<u8>>, PluginStoreError> {
        Ok(self.blobs.get(sha256.as_bytes())?.map(|v| v.to_vec()))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.write
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write_record(&self, record: &InstallRecord) -> Result<(), PluginStoreError> {
        self.installs
            .insert(record.id.as_bytes(), serde_json::to_vec(record)?)?;
        self.installs.flush()?;
        Ok(())
    }

    /// Store a new install; fails with `BlobMissing` when its module is gone
    /// (for instance deleted with the last other install while this one compiled).
    pub fn create(&self, record: &InstallRecord) -> Result<(), PluginStoreError> {
        let _guard = self.lock();
        if !self.blobs.contains_key(record.sha256.as_bytes())? {
            return Err(PluginStoreError::BlobMissing);
        }
        self.write_record(record)
    }

    pub fn get(&self, id: &str) -> Result<Option<InstallRecord>, PluginStoreError> {
        match self.installs.get(id.as_bytes())? {
            Some(v) => Ok(Some(serde_json::from_slice(&v)?)),
            None => Ok(None),
        }
    }

    /// All installs, oldest first (then by id).
    pub fn list(&self) -> Result<Vec<InstallRecord>, PluginStoreError> {
        let mut all = Vec::new();
        for item in self.installs.iter() {
            let (_, v) = item?;
            all.push(serde_json::from_slice::<InstallRecord>(&v)?);
        }
        all.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
        Ok(all)
    }

    pub fn set_enabled(
        &self,
        id: &str,
        enabled: bool,
    ) -> Result<Option<InstallRecord>, PluginStoreError> {
        let _guard = self.lock();
        let Some(mut record) = self.get(id)? else {
            return Ok(None);
        };
        record.enabled = enabled;
        self.write_record(&record)?;
        Ok(Some(record))
    }

    /// Remove an install; its blob goes too when no other install references it.
    pub fn delete(&self, id: &str) -> Result<bool, PluginStoreError> {
        let _guard = self.lock();
        let Some(record) = self.get(id)? else {
            return Ok(false);
        };
        self.installs.remove(id.as_bytes())?;
        if !self.list()?.iter().any(|r| r.sha256 == record.sha256) {
            self.blobs.remove(record.sha256.as_bytes())?;
        }
        self.installs.flush()?;
        self.blobs.flush()?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (PluginStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = sled::open(dir.path()).unwrap();
        (PluginStore::open(&db).unwrap(), dir)
    }

    fn record(id: &str, sha: &str, at: u64) -> InstallRecord {
        InstallRecord {
            id: id.into(),
            name: "demo".into(),
            sha256: sha.into(),
            size: 3,
            approved: Capabilities::parse(br#"{"log":true}"#).unwrap(),
            config: serde_json::json!({"a": 1}),
            enabled: true,
            created_at: at,
            created_by: Some("leandro".into()),
        }
    }

    #[test]
    fn plugins_are_served_only_when_enabled_standalone() {
        assert!(availability(true, false, false).is_ok());
        assert!(availability(false, false, false)
            .unwrap_err()
            .contains("--plugins"));
        assert!(availability(true, true, false)
            .unwrap_err()
            .contains("--ha-peers"));
        assert!(availability(true, false, true)
            .unwrap_err()
            .contains("slave"));
    }

    #[test]
    fn create_get_list_round_trip() {
        let (s, _d) = store();
        s.put_blob("sha1", b"abc").unwrap();
        s.create(&record("b", "sha1", 2)).unwrap();
        s.create(&record("a", "sha1", 1)).unwrap();
        assert_eq!(s.get("a").unwrap().unwrap(), record("a", "sha1", 1));
        assert!(s.get("zzz").unwrap().is_none());
        let ids: Vec<_> = s.list().unwrap().into_iter().map(|r| r.id).collect();
        assert_eq!(ids, ["a", "b"]);
    }

    #[test]
    fn set_enabled_updates_the_record() {
        let (s, _d) = store();
        s.put_blob("sha1", b"abc").unwrap();
        s.create(&record("a", "sha1", 1)).unwrap();
        assert!(!s.set_enabled("a", false).unwrap().unwrap().enabled);
        assert!(!s.get("a").unwrap().unwrap().enabled);
        assert!(s.set_enabled("nope", true).unwrap().is_none());
    }

    #[test]
    fn a_blob_is_kept_until_its_last_install_is_deleted() {
        let (s, _d) = store();
        s.put_blob("sha1", b"abc").unwrap();
        s.put_blob("sha1", b"abc").unwrap();
        s.create(&record("a", "sha1", 1)).unwrap();
        s.create(&record("b", "sha1", 2)).unwrap();
        assert!(s.delete("a").unwrap());
        assert_eq!(s.get_blob("sha1").unwrap().unwrap(), b"abc");
        assert!(s.delete("b").unwrap());
        assert!(s.get_blob("sha1").unwrap().is_none());
        assert!(!s.delete("b").unwrap());
    }

    #[test]
    fn create_after_the_last_delete_reports_the_missing_blob() {
        let (s, _d) = store();
        s.put_blob("sha1", b"abc").unwrap();
        s.create(&record("a", "sha1", 1)).unwrap();
        assert!(s.delete("a").unwrap());
        assert!(matches!(
            s.create(&record("b", "sha1", 2)),
            Err(PluginStoreError::BlobMissing)
        ));
        assert!(s.get("b").unwrap().is_none());
        assert!(s.set_enabled("a", true).unwrap().is_none());
        assert!(s.get("a").unwrap().is_none());
    }
}
