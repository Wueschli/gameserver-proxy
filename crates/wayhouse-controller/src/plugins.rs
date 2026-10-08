//! Plugin installs and module blobs (Wave 5; design:
//! `docs/superpowers/specs/2026-10-07-plugin-system-design.md`).
//!
//! A standalone controller keeps two sibling `sled` trees next to `revisions`:
//! `plugin_installs` (install id to [`InstallRecord`], JSON) and `plugin_blobs` (module
//! sha256 to the module bytes). A blob is content-addressed and shared by every install of
//! the same module; it is removed with the last install that references it. This slice
//! does not replicate anything: HA and slave controllers do not serve the plugin API.

pub mod api;
pub mod runner;

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use wayhouse_plugin_host::{Capabilities, StateSnapshot};

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

/// What applying a replicated install did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    Applied,
    /// A different install already has this id.
    Exists,
}

/// One install's state in a snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateSnap {
    pub id: String,
    pub rev: u64,
    pub entries: BTreeMap<String, Vec<u8>>,
}

/// Everything replicated about plugins, for an HA snapshot. Module blobs are not part
/// of it: they are content-addressed and fetched from a peer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginSnapshot {
    pub installs: Vec<InstallRecord>,
    pub state: Vec<StateSnap>,
}

/// Why a plugin's state could not be committed.
#[derive(Debug, thiserror::Error)]
pub enum CommitError {
    #[error("the plugin's state changed since the call read it")]
    Stale,
    #[error("no such install")]
    NoSuchInstall,
    #[error(transparent)]
    Store(#[from] PluginStoreError),
}

impl From<sled::Error> for CommitError {
    fn from(e: sled::Error) -> Self {
        Self::Store(e.into())
    }
}

/// State keys are `<install id>\0k:<key>`; the revision is `<install id>\0rev`.
fn state_prefix(id: &str) -> Vec<u8> {
    let mut p = id.as_bytes().to_vec();
    p.push(0);
    p
}

fn state_key(id: &str, key: &str) -> Vec<u8> {
    let mut k = state_prefix(id);
    k.extend_from_slice(b"k:");
    k.extend_from_slice(key.as_bytes());
    k
}

fn rev_key(id: &str) -> Vec<u8> {
    let mut k = state_prefix(id);
    k.extend_from_slice(b"rev");
    k
}

#[derive(Clone)]
pub struct PluginStore {
    installs: sled::Tree,
    blobs: sled::Tree,
    /// Plugin state: all installs share one tree so a commit is one atomic batch.
    state: sled::Tree,
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
            state: db.open_tree("plugin_state")?,
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

    fn read_rev(&self, id: &str) -> Result<u64, PluginStoreError> {
        Ok(self
            .state
            .get(rev_key(id))?
            .and_then(|v| <[u8; 8]>::try_from(v.as_ref()).ok())
            .map_or(0, u64::from_le_bytes))
    }

    /// An install's state and the revision it was read at (0 and empty when never written).
    pub fn state(&self, id: &str) -> Result<(u64, StateSnapshot), PluginStoreError> {
        let rev = self.read_rev(id)?;
        let mut prefix = state_prefix(id);
        prefix.extend_from_slice(b"k:");
        let mut snap = StateSnapshot::new();
        for item in self.state.scan_prefix(&prefix) {
            let (k, v) = item?;
            if let Ok(key) = std::str::from_utf8(&k[prefix.len()..]) {
                snap.insert(key.to_string(), v.to_vec());
            }
        }
        Ok((rev, snap))
    }

    /// Apply a call's `state_puts` as one batch if the state is still at `expected_rev`;
    /// returns the new revision. A stale or unknown-install commit changes nothing.
    pub fn commit_state(
        &self,
        id: &str,
        expected_rev: u64,
        puts: &BTreeMap<String, Vec<u8>>,
    ) -> Result<u64, CommitError> {
        let _guard = self.lock();
        if self.get(id)?.is_none() {
            return Err(CommitError::NoSuchInstall);
        }
        if self.read_rev(id)? != expected_rev {
            return Err(CommitError::Stale);
        }
        let next = expected_rev + 1;
        let mut batch = sled::Batch::default();
        for (k, v) in puts {
            batch.insert(state_key(id, k), v.as_slice());
        }
        batch.insert(rev_key(id), &next.to_le_bytes());
        self.state.apply_batch(batch)?;
        self.state.flush()?;
        Ok(next)
    }

    /// Apply a replicated `PluginInstall`: no blob check (the module travels out of
    /// band), and the same record delivered again counts as applied.
    pub fn apply_install(&self, record: &InstallRecord) -> Result<ApplyOutcome, PluginStoreError> {
        let _guard = self.lock();
        match self.get(&record.id)? {
            Some(existing) if existing == *record => Ok(ApplyOutcome::Applied),
            Some(_) => Ok(ApplyOutcome::Exists),
            None => {
                self.write_record(record)?;
                Ok(ApplyOutcome::Applied)
            }
        }
    }

    /// Every install and every install's state, in a stable order.
    pub fn snapshot(&self) -> Result<PluginSnapshot, PluginStoreError> {
        let _guard = self.lock();
        let installs = self.list()?;
        let mut state = Vec::new();
        for r in &installs {
            let (rev, entries) = self.state(&r.id)?;
            if rev > 0 || !entries.is_empty() {
                state.push(StateSnap {
                    id: r.id.clone(),
                    rev,
                    entries,
                });
            }
        }
        Ok(PluginSnapshot { installs, state })
    }

    /// Make the installs and state exactly `snap` (blobs untouched).
    pub fn replace(&self, snap: &PluginSnapshot) -> Result<(), PluginStoreError> {
        let _guard = self.lock();
        let mut installs = sled::Batch::default();
        for item in self.installs.iter() {
            installs.remove(item?.0);
        }
        for r in &snap.installs {
            installs.insert(r.id.as_bytes(), serde_json::to_vec(r)?);
        }
        self.installs.apply_batch(installs)?;
        let mut state = sled::Batch::default();
        for item in self.state.iter() {
            state.remove(item?.0);
        }
        for st in &snap.state {
            for (k, v) in &st.entries {
                state.insert(state_key(&st.id, k), v.as_slice());
            }
            state.insert(rev_key(&st.id), &st.rev.to_le_bytes());
        }
        self.state.apply_batch(state)?;
        self.installs.flush()?;
        self.state.flush()?;
        Ok(())
    }

    /// Remove an install; its blob goes too when no other install references it.
    pub fn delete(&self, id: &str) -> Result<bool, PluginStoreError> {
        let _guard = self.lock();
        let Some(record) = self.get(id)? else {
            return Ok(false);
        };
        self.installs.remove(id.as_bytes())?;
        let mut wipe = sled::Batch::default();
        for item in self.state.scan_prefix(state_prefix(id)) {
            wipe.remove(item?.0);
        }
        self.state.apply_batch(wipe)?;
        self.state.flush()?;
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

    fn puts(pairs: &[(&str, &[u8])]) -> std::collections::BTreeMap<String, Vec<u8>> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.to_vec()))
            .collect()
    }

    #[test]
    fn fresh_state_is_empty_at_revision_zero() {
        let (s, _d) = store();
        let (rev, snap) = s.state("a").unwrap();
        assert_eq!(rev, 0);
        assert!(snap.is_empty());
    }

    #[test]
    fn a_commit_at_the_current_revision_applies_and_bumps_it() {
        let (s, _d) = store();
        s.put_blob("sha1", b"abc").unwrap();
        s.create(&record("a", "sha1", 1)).unwrap();
        assert_eq!(
            s.commit_state("a", 0, &puts(&[("n", b"1"), ("m", b"2")]))
                .unwrap(),
            1
        );
        assert_eq!(s.commit_state("a", 1, &puts(&[("n", b"3")])).unwrap(), 2);
        let (rev, snap) = s.state("a").unwrap();
        assert_eq!(rev, 2);
        assert_eq!(snap, puts(&[("n", b"3"), ("m", b"2")]));
    }

    #[test]
    fn a_stale_commit_changes_nothing() {
        let (s, _d) = store();
        s.put_blob("sha1", b"abc").unwrap();
        s.create(&record("a", "sha1", 1)).unwrap();
        s.commit_state("a", 0, &puts(&[("n", b"1")])).unwrap();
        assert!(matches!(
            s.commit_state("a", 0, &puts(&[("n", b"9")])),
            Err(CommitError::Stale)
        ));
        assert_eq!(s.state("a").unwrap().1, puts(&[("n", b"1")]));
    }

    #[test]
    fn committing_for_an_unknown_install_is_refused() {
        let (s, _d) = store();
        assert!(matches!(
            s.commit_state("nope", 0, &puts(&[("n", b"1")])),
            Err(CommitError::NoSuchInstall)
        ));
        assert_eq!(s.state("nope").unwrap().0, 0);
    }

    #[test]
    fn deleting_an_install_removes_only_its_state() {
        let (s, _d) = store();
        s.put_blob("sha1", b"abc").unwrap();
        s.create(&record("a", "sha1", 1)).unwrap();
        s.create(&record("ab", "sha1", 2)).unwrap();
        s.commit_state("a", 0, &puts(&[("n", b"1")])).unwrap();
        s.commit_state("ab", 0, &puts(&[("n", b"2")])).unwrap();
        assert!(s.delete("a").unwrap());
        assert_eq!(s.state("a").unwrap().0, 0);
        assert!(s.state("a").unwrap().1.is_empty());
        assert_eq!(s.state("ab").unwrap().1, puts(&[("n", b"2")]));
    }

    #[test]
    fn apply_install_needs_no_blob_and_is_idempotent() {
        let (s, _d) = store();
        assert_eq!(
            s.apply_install(&record("a", "sha1", 1)).unwrap(),
            ApplyOutcome::Applied
        );
        assert_eq!(s.get("a").unwrap().unwrap(), record("a", "sha1", 1));
        // The same entry delivered again after a crash is harmless.
        assert_eq!(
            s.apply_install(&record("a", "sha1", 1)).unwrap(),
            ApplyOutcome::Applied
        );
        // A different record under the same id is refused, not overwritten.
        assert_eq!(
            s.apply_install(&record("a", "sha2", 9)).unwrap(),
            ApplyOutcome::Exists
        );
        assert_eq!(s.get("a").unwrap().unwrap(), record("a", "sha1", 1));
    }

    #[test]
    fn a_snapshot_reproduces_installs_state_and_revisions() {
        let (src, _d1) = store();
        src.apply_install(&record("a", "sha1", 1)).unwrap();
        src.apply_install(&record("b", "sha1", 2)).unwrap();
        src.commit_state("a", 0, &puts(&[("n", b"1")])).unwrap();
        src.commit_state("a", 1, &puts(&[("m", b"2")])).unwrap();
        let snap = src.snapshot().unwrap();

        let (dst, _d2) = store();
        dst.apply_install(&record("old", "sha9", 5)).unwrap();
        dst.commit_state("old", 0, &puts(&[("x", b"1")])).unwrap();
        dst.replace(&snap).unwrap();
        assert_eq!(dst.snapshot().unwrap(), snap);
        assert!(dst.get("old").unwrap().is_none());
        assert_eq!(
            dst.state("old").unwrap().0,
            0,
            "state outside the snapshot is gone"
        );
        assert_eq!(dst.state("a").unwrap(), src.state("a").unwrap());
        assert_eq!(dst.state("a").unwrap().0, 2);
    }

    #[test]
    fn replacing_from_a_snapshot_leaves_blobs_alone() {
        let (s, _d) = store();
        s.put_blob("sha1", b"abc").unwrap();
        s.replace(&PluginSnapshot::default()).unwrap();
        assert_eq!(s.get_blob("sha1").unwrap().unwrap(), b"abc");
    }
}
