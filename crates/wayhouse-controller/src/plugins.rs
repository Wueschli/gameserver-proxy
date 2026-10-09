//! Plugin installs and module blobs (Wave 5; design:
//! `docs/superpowers/specs/2026-10-07-plugin-system-design.md`).
//!
//! A standalone controller keeps two sibling `sled` trees next to `revisions`:
//! `plugin_installs` (install id to [`InstallRecord`], JSON) and `plugin_blobs` (module
//! sha256 to the module bytes). A blob is content-addressed and shared by every install of
//! the same module; it is removed with the last install that references it. A standalone
//! controller writes through [`PluginStore`]'s direct methods; under HA the Raft state
//! machine is the only writer and goes through [`PluginStore::apply_at`], which is
//! crash-idempotent per Raft index. Slave controllers do not serve the plugin API.

pub mod api;
pub mod net;
pub mod peer;
pub mod runner;
pub mod secrets;

use serde::{Deserialize, Serialize};
use sled::transaction::{ConflictableTransactionError, TransactionError};
use sled::Transactional;
use std::collections::BTreeMap;

use secrets::Sealed;
use wayhouse_plugin_host::{
    Capabilities, StateSnapshot, MAX_KEY_BYTES, MAX_MODULE_BYTES, MAX_STATE_BYTES,
};

/// Largest `config` an install may carry, serialized.
pub const MAX_CONFIG_BYTES: usize = 64 * 1024;
/// `state_put`s one call may commit (the host's per-call limit).
pub const MAX_PUTS: usize = 256;

/// Whether this controller serves the plugin API: `Err` carries the reason it does not.
/// Plugins are opt-in (`--plugins`); a slave tier has none (they run on the root tier).
pub fn availability(enabled: bool, slave: bool) -> Result<(), &'static str> {
    if !enabled {
        Err("plugins are off; start the controller with --plugins")
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

/// Whether `name` is a valid install name.
pub fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Whether `id` has the shape of an install id (16 lowercase hex characters).
pub fn valid_id(id: &str) -> bool {
    is_lower_hex(id, 16)
}

/// Whether `s` has the shape of a module hash (64 lowercase hex characters).
pub fn is_sha256(s: &str) -> bool {
    is_lower_hex(s, 64)
}

fn is_lower_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

impl InstallRecord {
    /// The checks the API makes before an install and every replica makes when it
    /// applies one, so a malformed record from any proposer is refused the same way
    /// everywhere. (The approved capabilities are typed and validated on parse.)
    pub fn validate(&self) -> Result<(), String> {
        if !valid_id(&self.id) {
            return Err("id must be 16 lowercase hex characters".into());
        }
        if !valid_name(&self.name) {
            return Err(
                "name must be 1 to 64 characters of a-z, 0-9 and '-', not starting with '-'".into(),
            );
        }
        if !is_sha256(&self.sha256) {
            return Err("sha256 must be 64 lowercase hex characters".into());
        }
        if self.size == 0 || self.size > MAX_MODULE_BYTES {
            return Err(format!("size must be 1 to {MAX_MODULE_BYTES} bytes"));
        }
        if self
            .created_by
            .as_ref()
            .is_some_and(|a| a.chars().count() > 128)
        {
            return Err("created_by is over 128 characters".into());
        }
        if serde_json::to_vec(&self.config).map_or(true, |b| b.len() > MAX_CONFIG_BYTES) {
            return Err(format!("config is over {MAX_CONFIG_BYTES} bytes"));
        }
        Ok(())
    }
}

/// The bounds a call's `state_put`s already meet in the host, checked again where they
/// become a log entry (propose) and where it is applied, so a bad proposer cannot put
/// an unbounded entry in every replica's log.
pub fn validate_puts(puts: &BTreeMap<String, Vec<u8>>) -> Result<(), String> {
    if puts.len() > MAX_PUTS {
        return Err(format!("more than {MAX_PUTS} state keys in one commit"));
    }
    let mut total = 0usize;
    for (k, v) in puts {
        if k.len() > MAX_KEY_BYTES {
            return Err(format!("a state key is over {MAX_KEY_BYTES} bytes"));
        }
        total += k.len() + v.len();
    }
    if total > MAX_STATE_BYTES {
        return Err(format!("state writes are over {MAX_STATE_BYTES} bytes"));
    }
    Ok(())
}

/// Byte values as base64 strings, so a snapshot or log entry costs a third more than the
/// bytes, not several times as a JSON number array does.
pub mod b64_entries {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    use serde::{Deserialize, Deserializer, Serializer};
    use std::collections::BTreeMap;

    pub fn serialize<S: Serializer>(
        map: &BTreeMap<String, Vec<u8>>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        s.collect_map(map.iter().map(|(k, v)| (k, STANDARD.encode(v))))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<BTreeMap<String, Vec<u8>>, D::Error> {
        BTreeMap::<String, String>::deserialize(d)?
            .into_iter()
            .map(|(k, v)| {
                STANDARD
                    .decode(v)
                    .map(|b| (k, b))
                    .map_err(serde::de::Error::custom)
            })
            .collect()
    }
}

/// One install's state in a snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateSnap {
    pub id: String,
    pub rev: u64,
    #[serde(with = "b64_entries")]
    pub entries: BTreeMap<String, Vec<u8>>,
}

/// A stored secret: ciphertext only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredSecret {
    pub sealed: Sealed,
    pub updated_at: u64,
}

/// One secret in a snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretSnap {
    pub id: String,
    pub slot: String,
    #[serde(flatten)]
    pub stored: StoredSecret,
}

/// One secret to re-encrypt in a rewrap: applied only if the slot still holds the
/// ciphertext (`from_nonce`) the new one was derived from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewrapItem {
    pub id: String,
    pub slot: String,
    pub from_nonce: String,
    pub sealed: Sealed,
}

/// Checks a sealed secret's shape, so a malformed one from any proposer is refused alike
/// on every replica.
pub fn validate_sealed(sealed: &Sealed) -> Result<(), String> {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    if !is_lower_hex(&sealed.key_id, 16) {
        return Err("key_id must be 16 lowercase hex characters".into());
    }
    if STANDARD.decode(&sealed.nonce).map(|n| n.len()) != Ok(24) {
        return Err("nonce must be 24 bytes of base64".into());
    }
    match STANDARD.decode(&sealed.ciphertext) {
        Ok(c) if (16..=secrets::MAX_SECRET_BYTES + 16).contains(&c.len()) => Ok(()),
        _ => Err("ciphertext has an invalid size".into()),
    }
}

fn secret_key(id: &str, slot: &str) -> Vec<u8> {
    let mut k = state_prefix(id);
    k.extend_from_slice(slot.as_bytes());
    k
}

/// Everything replicated about plugins, for an HA snapshot. Module blobs are not part
/// of it: they are content-addressed and fetched from a peer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginSnapshot {
    pub installs: Vec<InstallRecord>,
    pub state: Vec<StateSnap>,
    /// The highest Raft index applied to the plugin trees: entries at or below it are
    /// replays after a snapshot install or a crash and change nothing.
    #[serde(default)]
    pub applied_index: Option<u64>,
    /// Secrets, as ciphertext.
    #[serde(default)]
    pub secrets: Vec<SecretSnap>,
}

/// A replicated plugin write, applied by [`PluginStore::apply_at`].
#[derive(Debug, Clone, Copy)]
pub enum PluginOp<'a> {
    Install(&'a InstallRecord),
    SetEnabled {
        id: &'a str,
        enabled: bool,
    },
    Delete {
        id: &'a str,
    },
    State {
        id: &'a str,
        expected_rev: u64,
        /// The term the proposing leader read the state in.
        tagged_term: u64,
        /// The term the entry was appended in.
        entry_term: u64,
        puts: &'a BTreeMap<String, Vec<u8>>,
    },
    SecretSet {
        id: &'a str,
        slot: &'a str,
        sealed: &'a Sealed,
        updated_at: u64,
    },
    SecretDelete {
        id: &'a str,
        slot: &'a str,
    },
    SecretRewrap {
        items: &'a [RewrapItem],
        updated_at: u64,
    },
}

/// What a replicated write did. Everything but `Replayed` and `Done` changed nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied {
    /// The entry's index was already applied (a replay): nothing was done.
    Replayed,
    /// Applied; a state commit carries the install's new state revision.
    Done(Option<u64>),
    /// A different install already has this id.
    Exists,
    NoSuchInstall,
    /// The state revision moved since the call read it.
    Stale,
    /// The entry was not appended in the term the call was proposed in.
    WrongTerm,
    /// The record or writes are malformed; the reason is for the log only.
    Invalid(String),
    /// A rewrap was applied; the `install/slot` of every item that was skipped because
    /// the slot changed since the ciphertext was read.
    Rewrapped(Vec<String>),
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

/// Key of the Raft index cursor in the `plugin_meta` tree.
const APPLIED_KEY: &[u8] = b"applied";

#[derive(Clone)]
pub struct PluginStore {
    installs: sled::Tree,
    blobs: sled::Tree,
    /// Plugin state: all installs share one tree so a commit is one atomic batch.
    state: sled::Tree,
    /// `applied`: the highest Raft index applied (big-endian `u64`), written in the same
    /// transaction as the entry it covers.
    meta: sled::Tree,
    /// `<install id>\0<slot>` to a [`StoredSecret`] (JSON): ciphertext only.
    secrets: sled::Tree,
    /// Serialises create, set_enabled and delete, so a blob cannot be
    /// garbage-collected between an install's blob check and its write.
    write: std::sync::Arc<std::sync::Mutex<()>>,
}

impl PluginStore {
    /// Open the trees in the controller's database.
    pub fn open(db: &sled::Db) -> Result<Self, sled::Error> {
        Ok(Self {
            installs: db.open_tree("plugin_installs")?,
            blobs: db.open_tree("plugin_blobs")?,
            state: db.open_tree("plugin_state")?,
            meta: db.open_tree("plugin_meta")?,
            secrets: db.open_tree("plugin_secrets")?,
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

    /// The highest Raft index applied through [`PluginStore::apply_at`] or restored from a
    /// snapshot.
    pub fn applied_index(&self) -> Result<Option<u64>, PluginStoreError> {
        Ok(self
            .meta
            .get(APPLIED_KEY)?
            .and_then(|v| <[u8; 8]>::try_from(v.as_ref()).ok())
            .map(u64::from_be_bytes))
    }

    /// Apply a replicated write at Raft `index`. The entry's effect and the cursor commit in
    /// one transaction, so a crash leaves either both or neither; an entry at or below the
    /// cursor is a replay and changes nothing. Rejections advance the cursor too (they
    /// are the same answer on every replica, so replaying them is pointless).
    ///
    /// No blob check: the module travels out of band. A delete removes the module blob too
    /// once no other install references it.
    pub fn apply_at(&self, index: u64, op: PluginOp<'_>) -> Result<Applied, PluginStoreError> {
        let _guard = self.lock();
        // Keys to wipe with a deleted install: the transaction cannot scan, and the lock
        // keeps the set from changing under it.
        let (wipe, wipe_secrets): (Vec<sled::IVec>, Vec<sled::IVec>) = match op {
            PluginOp::Delete { id } => (
                self.state
                    .scan_prefix(state_prefix(id))
                    .keys()
                    .collect::<Result<_, _>>()?,
                self.secrets
                    .scan_prefix(state_prefix(id))
                    .keys()
                    .collect::<Result<_, _>>()?,
            ),
            _ => (Vec::new(), Vec::new()),
        };
        let abort = |e: PluginStoreError| ConflictableTransactionError::Abort(e);
        let outcome = (&self.installs, &self.state, &self.meta, &self.secrets).transaction(
            |(inst, st, meta, sec)| {
                let covered = meta
                    .get(APPLIED_KEY)?
                    .and_then(|v| <[u8; 8]>::try_from(v.as_ref()).ok())
                    .map(u64::from_be_bytes);
                if covered.is_some_and(|c| c >= index) {
                    return Ok((Applied::Replayed, None));
                }
                meta.insert(APPLIED_KEY, &index.to_be_bytes())?;
                let read =
                    |id: &str| -> Result<Option<InstallRecord>, ConflictableTransactionError<_>> {
                        match inst.get(id.as_bytes())? {
                            Some(v) => Ok(Some(
                                serde_json::from_slice(&v).map_err(|e| abort(e.into()))?,
                            )),
                            None => Ok(None),
                        }
                    };
                let done = |a| Ok((a, None));
                match op {
                    PluginOp::Install(record) => {
                        if let Err(why) = record.validate() {
                            return done(Applied::Invalid(why));
                        }
                        if read(&record.id)?.is_some() {
                            return done(Applied::Exists);
                        }
                        let bytes = serde_json::to_vec(record).map_err(|e| abort(e.into()))?;
                        inst.insert(record.id.as_bytes(), bytes)?;
                        done(Applied::Done(None))
                    }
                    PluginOp::SetEnabled { id, enabled } => {
                        let Some(mut record) = read(id)? else {
                            return done(Applied::NoSuchInstall);
                        };
                        record.enabled = enabled;
                        let bytes = serde_json::to_vec(&record).map_err(|e| abort(e.into()))?;
                        inst.insert(id.as_bytes(), bytes)?;
                        done(Applied::Done(None))
                    }
                    PluginOp::Delete { id } => {
                        let Some(record) = read(id)? else {
                            return done(Applied::NoSuchInstall);
                        };
                        inst.remove(id.as_bytes())?;
                        for key in &wipe {
                            st.remove(key.clone())?;
                        }
                        for key in &wipe_secrets {
                            sec.remove(key.clone())?;
                        }
                        Ok((Applied::Done(None), Some(record.sha256)))
                    }
                    PluginOp::State {
                        id,
                        expected_rev,
                        tagged_term,
                        entry_term,
                        puts,
                    } => {
                        if tagged_term != entry_term {
                            return done(Applied::WrongTerm);
                        }
                        if let Err(why) = validate_puts(puts) {
                            return done(Applied::Invalid(why));
                        }
                        if read(id)?.is_none() {
                            return done(Applied::NoSuchInstall);
                        }
                        let current = st
                            .get(rev_key(id))?
                            .and_then(|v| <[u8; 8]>::try_from(v.as_ref()).ok())
                            .map_or(0, u64::from_le_bytes);
                        if current != expected_rev {
                            return done(Applied::Stale);
                        }
                        let next = current + 1;
                        for (k, v) in puts {
                            st.insert(state_key(id, k), v.as_slice())?;
                        }
                        st.insert(rev_key(id), &next.to_le_bytes())?;
                        done(Applied::Done(Some(next)))
                    }
                    PluginOp::SecretSet {
                        id,
                        slot,
                        sealed,
                        updated_at,
                    } => {
                        let Some(record) = read(id)? else {
                            return done(Applied::NoSuchInstall);
                        };
                        if !record.approved.secrets.iter().any(|s| s.name == slot) {
                            return done(Applied::Invalid(format!("slot {slot} is not approved")));
                        }
                        if let Err(why) = validate_sealed(sealed) {
                            return done(Applied::Invalid(why));
                        }
                        let stored = StoredSecret {
                            sealed: sealed.clone(),
                            updated_at,
                        };
                        let bytes = serde_json::to_vec(&stored).map_err(|e| abort(e.into()))?;
                        sec.insert(secret_key(id, slot), bytes)?;
                        done(Applied::Done(None))
                    }
                    PluginOp::SecretDelete { id, slot } => {
                        if read(id)?.is_none() {
                            return done(Applied::NoSuchInstall);
                        }
                        sec.remove(secret_key(id, slot))?;
                        done(Applied::Done(None))
                    }
                    PluginOp::SecretRewrap { items, updated_at } => {
                        let mut skipped = Vec::new();
                        for item in items {
                            let key = secret_key(&item.id, &item.slot);
                            let current: Option<StoredSecret> = match sec.get(&key)? {
                                Some(v) => {
                                    Some(serde_json::from_slice(&v).map_err(|e| abort(e.into()))?)
                                }
                                None => None,
                            };
                            let unchanged = current
                                .as_ref()
                                .is_some_and(|c| c.sealed.nonce == item.from_nonce);
                            if !unchanged || validate_sealed(&item.sealed).is_err() {
                                skipped.push(format!("{}/{}", item.id, item.slot));
                                continue;
                            }
                            let stored = StoredSecret {
                                sealed: item.sealed.clone(),
                                updated_at,
                            };
                            let bytes = serde_json::to_vec(&stored).map_err(|e| abort(e.into()))?;
                            sec.insert(key, bytes)?;
                        }
                        done(Applied::Rewrapped(skipped))
                    }
                }
            },
        );
        let (applied, freed) = outcome.map_err(|e| match e {
            TransactionError::Abort(e) => e,
            TransactionError::Storage(e) => e.into(),
        })?;
        self.installs.flush()?;
        self.state.flush()?;
        self.meta.flush()?;
        self.secrets.flush()?;
        if let Some(sha) = freed {
            if !self.list()?.iter().any(|r| r.sha256 == sha) {
                self.blobs.remove(sha.as_bytes())?;
                self.blobs.flush()?;
            }
        }
        Ok(applied)
    }

    /// Every install and every install's state, in a stable order, with the cursor they
    /// are at. Taken on the state-machine worker, which serialises it with `apply`, so it
    /// is at exactly the snapshot's last applied index.
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
        Ok(PluginSnapshot {
            installs,
            state,
            applied_index: self.applied_index()?,
            secrets: self.all_secrets()?,
        })
    }

    /// Make the installs, state and cursor exactly `snap` (blobs untouched), in one
    /// transaction: a crash leaves the old content or the new, never a mix.
    pub fn replace(&self, snap: &PluginSnapshot) -> Result<(), PluginStoreError> {
        let _guard = self.lock();
        let old_installs: Vec<sled::IVec> =
            self.installs.iter().keys().collect::<Result<_, _>>()?;
        let old_state: Vec<sled::IVec> = self.state.iter().keys().collect::<Result<_, _>>()?;
        let old_secrets: Vec<sled::IVec> = self.secrets.iter().keys().collect::<Result<_, _>>()?;
        let mut new_secrets = Vec::new();
        for sec in &snap.secrets {
            new_secrets.push((
                secret_key(&sec.id, &sec.slot),
                serde_json::to_vec(&sec.stored)?,
            ));
        }
        let mut new_installs = Vec::new();
        for r in &snap.installs {
            new_installs.push((r.id.as_bytes().to_vec(), serde_json::to_vec(r)?));
        }
        let mut new_state = Vec::new();
        for st in &snap.state {
            for (k, v) in &st.entries {
                new_state.push((state_key(&st.id, k), v.clone()));
            }
            new_state.push((rev_key(&st.id), st.rev.to_le_bytes().to_vec()));
        }
        (&self.installs, &self.state, &self.meta, &self.secrets)
            .transaction(|(inst, st, meta, sec)| {
                for k in &old_secrets {
                    sec.remove(k.clone())?;
                }
                for (k, v) in &new_secrets {
                    sec.insert(k.as_slice(), v.as_slice())?;
                }
                for k in &old_installs {
                    inst.remove(k.clone())?;
                }
                for (k, v) in &new_installs {
                    inst.insert(k.as_slice(), v.as_slice())?;
                }
                for k in &old_state {
                    st.remove(k.clone())?;
                }
                for (k, v) in &new_state {
                    st.insert(k.as_slice(), v.as_slice())?;
                }
                match snap.applied_index {
                    Some(i) => meta.insert(APPLIED_KEY, &i.to_be_bytes())?,
                    None => meta.remove(APPLIED_KEY)?,
                };
                Ok::<_, ConflictableTransactionError<PluginStoreError>>(())
            })
            .map_err(|e| match e {
                TransactionError::Abort(e) => e,
                TransactionError::Storage(e) => e.into(),
            })?;
        self.installs.flush()?;
        self.state.flush()?;
        self.meta.flush()?;
        self.secrets.flush()?;
        Ok(())
    }

    /// Stores a sealed secret on a standalone controller (under HA the state machine
    /// applies `SecretSet`). The install must exist and have approved the slot.
    pub fn put_secret(
        &self,
        id: &str,
        slot: &str,
        sealed: &Sealed,
        updated_at: u64,
    ) -> Result<Applied, PluginStoreError> {
        let _guard = self.lock();
        let Some(record) = self.get(id)? else {
            return Ok(Applied::NoSuchInstall);
        };
        if !record.approved.secrets.iter().any(|s| s.name == slot) {
            return Ok(Applied::Invalid(format!("slot {slot} is not approved")));
        }
        if let Err(why) = validate_sealed(sealed) {
            return Ok(Applied::Invalid(why));
        }
        let stored = StoredSecret {
            sealed: sealed.clone(),
            updated_at,
        };
        self.secrets
            .insert(secret_key(id, slot), serde_json::to_vec(&stored)?)?;
        self.secrets.flush()?;
        Ok(Applied::Done(None))
    }

    /// Standalone rewrap: re-encrypts each item whose slot still holds the ciphertext it
    /// was derived from; returns the `install/slot` of those skipped.
    pub fn rewrap(
        &self,
        items: &[RewrapItem],
        updated_at: u64,
    ) -> Result<Vec<String>, PluginStoreError> {
        let _guard = self.lock();
        let mut skipped = Vec::new();
        for item in items {
            let key = secret_key(&item.id, &item.slot);
            let current = self.get_secret(&item.id, &item.slot)?;
            let unchanged = current.is_some_and(|c| c.sealed.nonce == item.from_nonce);
            if !unchanged || validate_sealed(&item.sealed).is_err() {
                skipped.push(format!("{}/{}", item.id, item.slot));
                continue;
            }
            let stored = StoredSecret {
                sealed: item.sealed.clone(),
                updated_at,
            };
            self.secrets.insert(key, serde_json::to_vec(&stored)?)?;
        }
        self.secrets.flush()?;
        Ok(skipped)
    }

    /// Standalone `DELETE` of a secret; `false` when the install does not exist.
    pub fn delete_secret(&self, id: &str, slot: &str) -> Result<bool, PluginStoreError> {
        let _guard = self.lock();
        if self.get(id)?.is_none() {
            return Ok(false);
        }
        self.secrets.remove(secret_key(id, slot))?;
        self.secrets.flush()?;
        Ok(true)
    }

    pub fn get_secret(
        &self,
        id: &str,
        slot: &str,
    ) -> Result<Option<StoredSecret>, PluginStoreError> {
        match self.secrets.get(secret_key(id, slot))? {
            Some(v) => Ok(Some(serde_json::from_slice(&v)?)),
            None => Ok(None),
        }
    }

    /// Every stored secret, ordered by install and slot.
    pub fn all_secrets(&self) -> Result<Vec<SecretSnap>, PluginStoreError> {
        let mut out = Vec::new();
        for item in self.secrets.iter() {
            let (k, v) = item?;
            let Some(at) = k.iter().position(|b| *b == 0) else {
                continue;
            };
            let (Ok(id), Ok(slot)) = (
                std::str::from_utf8(&k[..at]),
                std::str::from_utf8(&k[at + 1..]),
            ) else {
                continue;
            };
            out.push(SecretSnap {
                id: id.into(),
                slot: slot.into(),
                stored: serde_json::from_slice(&v)?,
            });
        }
        Ok(out)
    }

    /// Remove every module blob no install references: blobs of uploads that were never
    /// installed, and of installs deleted while a crash interrupted the cleanup after
    /// their transaction. Run at startup; returns how many were removed.
    pub fn sweep_blobs(&self) -> Result<usize, PluginStoreError> {
        let _guard = self.lock();
        let referenced: std::collections::HashSet<String> =
            self.list()?.into_iter().map(|r| r.sha256).collect();
        let mut removed = 0;
        for key in self.blobs.iter().keys() {
            let key = key?;
            let kept = std::str::from_utf8(&key).is_ok_and(|sha| referenced.contains(sha));
            if !kept {
                self.blobs.remove(&key)?;
                removed += 1;
            }
        }
        self.blobs.flush()?;
        Ok(removed)
    }

    /// The modules some install references that this node does not hold (each once): what
    /// a replica fetches from its peers. Replicated installs arrive before their module.
    pub fn missing_blobs(&self) -> Result<Vec<String>, PluginStoreError> {
        let mut missing = std::collections::BTreeSet::new();
        for r in self.list()? {
            if !self.blobs.contains_key(r.sha256.as_bytes())? {
                missing.insert(r.sha256);
            }
        }
        Ok(missing.into_iter().collect())
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
        let mut wipe_secrets = sled::Batch::default();
        for item in self.secrets.scan_prefix(state_prefix(id)) {
            wipe_secrets.remove(item?.0);
        }
        self.secrets.apply_batch(wipe_secrets)?;
        self.secrets.flush()?;
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
    fn plugins_are_served_when_enabled_except_on_a_slave_tier() {
        assert!(availability(true, false).is_ok());
        assert!(availability(false, false)
            .unwrap_err()
            .contains("--plugins"));
        assert!(availability(true, true).unwrap_err().contains("slave"));
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
    fn a_sweep_removes_only_blobs_no_install_references() {
        let (s, _d) = store();
        s.put_blob("kept", b"abc").unwrap();
        s.put_blob("orphan", b"def").unwrap();
        s.create(&record("a", "kept", 1)).unwrap();
        assert_eq!(s.sweep_blobs().unwrap(), 1);
        assert!(s.get_blob("kept").unwrap().is_some());
        assert!(s.get_blob("orphan").unwrap().is_none());
        assert_eq!(s.sweep_blobs().unwrap(), 0);
    }

    #[test]
    fn missing_blobs_lists_each_referenced_module_this_node_lacks_once() {
        let (s, _d) = store();
        s.put_blob("here", b"abc").unwrap();
        for (id, sha, at) in [("a", "gone", 1), ("b", "gone", 2), ("c", "here", 3)] {
            // Replicated installs arrive without a blob, so bypass `create`'s check.
            s.write_record(&record(id, sha, at)).unwrap();
        }
        assert_eq!(s.missing_blobs().unwrap(), ["gone"]);
        s.put_blob("gone", b"xyz").unwrap();
        assert!(s.missing_blobs().unwrap().is_empty());
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

    fn valid(id: &str, sha_digit: char, at: u64) -> InstallRecord {
        let mut r = record(id, &sha_digit.to_string().repeat(64), at);
        r.size = 3;
        r
    }

    const ID_A: &str = "00000000000000aa";
    const ID_B: &str = "00000000000000bb";

    fn put_state(s: &PluginStore, index: u64, id: &str, rev: u64, k: &str, v: &[u8]) -> Applied {
        s.apply_at(
            index,
            PluginOp::State {
                id,
                expected_rev: rev,
                tagged_term: 3,
                entry_term: 3,
                puts: &puts(&[(k, v)]),
            },
        )
        .unwrap()
    }

    #[test]
    fn apply_install_needs_no_blob_and_a_duplicate_id_is_refused() {
        let (s, _d) = store();
        let a = valid(ID_A, 'a', 1);
        assert_eq!(
            s.apply_at(1, PluginOp::Install(&a)).unwrap(),
            Applied::Done(None)
        );
        assert_eq!(s.get(ID_A).unwrap().unwrap(), a);
        // A different record under the same id is refused, not overwritten.
        let other = valid(ID_A, 'b', 9);
        assert_eq!(
            s.apply_at(2, PluginOp::Install(&other)).unwrap(),
            Applied::Exists
        );
        assert_eq!(s.get(ID_A).unwrap().unwrap(), a);
    }

    #[test]
    fn an_applied_index_makes_replays_change_nothing_and_answer_replayed() {
        let (s, _d) = store();
        let a = valid(ID_A, 'a', 1);
        s.apply_at(1, PluginOp::Install(&a)).unwrap();
        assert_eq!(put_state(&s, 2, ID_A, 0, "n", b"1"), Applied::Done(Some(1)));
        assert_eq!(s.applied_index().unwrap(), Some(2));
        // openraft re-delivers entries after a crash lost its own cursor.
        assert_eq!(
            s.apply_at(1, PluginOp::Install(&a)).unwrap(),
            Applied::Replayed
        );
        assert_eq!(put_state(&s, 2, ID_A, 0, "n", b"9"), Applied::Replayed);
        let (rev, state) = s.state(ID_A).unwrap();
        assert_eq!((rev, state), (1, puts(&[("n", b"1")])));
        assert_eq!(s.applied_index().unwrap(), Some(2));
    }

    #[test]
    fn a_rejected_entry_still_advances_the_cursor() {
        let (s, _d) = store();
        assert_eq!(
            put_state(&s, 5, "00000000000000cc", 0, "n", b"1"),
            Applied::NoSuchInstall
        );
        assert_eq!(s.applied_index().unwrap(), Some(5));
    }

    #[test]
    fn state_entries_report_stale_wrong_term_and_the_new_revision() {
        let (s, _d) = store();
        s.apply_at(1, PluginOp::Install(&valid(ID_A, 'a', 1)))
            .unwrap();
        assert_eq!(put_state(&s, 2, ID_A, 0, "n", b"1"), Applied::Done(Some(1)));
        assert_eq!(put_state(&s, 3, ID_A, 0, "n", b"9"), Applied::Stale);
        let wrong = s
            .apply_at(
                4,
                PluginOp::State {
                    id: ID_A,
                    expected_rev: 1,
                    tagged_term: 2,
                    entry_term: 3,
                    puts: &puts(&[("n", b"9")]),
                },
            )
            .unwrap();
        assert_eq!(wrong, Applied::WrongTerm);
        assert_eq!(s.state(ID_A).unwrap().1, puts(&[("n", b"1")]));
    }

    #[test]
    fn set_enabled_and_delete_apply_through_the_cursor() {
        let (s, _d) = store();
        s.apply_at(1, PluginOp::Install(&valid(ID_A, 'a', 1)))
            .unwrap();
        put_state(&s, 2, ID_A, 0, "n", b"1");
        assert_eq!(
            s.apply_at(
                3,
                PluginOp::SetEnabled {
                    id: ID_A,
                    enabled: false
                }
            )
            .unwrap(),
            Applied::Done(None)
        );
        assert!(!s.get(ID_A).unwrap().unwrap().enabled);
        assert_eq!(
            s.apply_at(4, PluginOp::Delete { id: ID_A }).unwrap(),
            Applied::Done(None)
        );
        assert!(s.get(ID_A).unwrap().is_none());
        assert_eq!(s.state(ID_A).unwrap().0, 0);
        assert_eq!(
            s.apply_at(5, PluginOp::Delete { id: ID_A }).unwrap(),
            Applied::NoSuchInstall
        );
        assert_eq!(
            s.apply_at(
                6,
                PluginOp::SetEnabled {
                    id: ID_A,
                    enabled: true
                }
            )
            .unwrap(),
            Applied::NoSuchInstall
        );
    }

    #[test]
    fn a_malformed_install_is_refused_deterministically() {
        let (s, _d) = store();
        let mut bad = valid(ID_A, 'a', 1);
        bad.id = "../x".into();
        assert!(matches!(
            s.apply_at(1, PluginOp::Install(&bad)).unwrap(),
            Applied::Invalid(_)
        ));
        assert!(s.get("../x").unwrap().is_none());
        assert_eq!(s.applied_index().unwrap(), Some(1));
    }

    #[test]
    fn install_validation_names_each_bad_field() {
        let ok = valid(ID_A, 'a', 1);
        assert!(ok.validate().is_ok());
        let check = |edit: fn(&mut InstallRecord), word: &str| {
            let mut r = ok.clone();
            edit(&mut r);
            let e = r.validate().unwrap_err();
            assert!(e.contains(word), "{e}");
        };
        check(|r| r.id = "short".into(), "id");
        check(|r| r.name = "Bad Name".into(), "name");
        check(|r| r.sha256 = "xyz".into(), "sha256");
        check(|r| r.size = 0, "size");
        check(|r| r.size = MAX_MODULE_BYTES + 1, "size");
        check(|r| r.created_by = Some("x".repeat(129)), "created_by");
        check(
            |r| r.config = serde_json::json!({"k": "x".repeat(MAX_CONFIG_BYTES)}),
            "config",
        );
    }

    #[test]
    fn state_puts_are_bounded() {
        assert!(validate_puts(&puts(&[("n", b"1")])).is_ok());
        let long_key = "k".repeat(MAX_KEY_BYTES + 1);
        assert!(validate_puts(&puts(&[(long_key.as_str(), b"1")])).is_err());
        let big = vec![0u8; MAX_STATE_BYTES + 1];
        assert!(validate_puts(&puts(&[("n", big.as_slice())])).is_err());
        let many: BTreeMap<String, Vec<u8>> =
            (0..257).map(|i| (format!("k{i}"), vec![1])).collect();
        assert!(validate_puts(&many).is_err());
    }

    #[test]
    fn a_snapshot_reproduces_installs_state_revisions_and_the_cursor() {
        let (src, _d1) = store();
        src.apply_at(1, PluginOp::Install(&valid(ID_A, 'a', 1)))
            .unwrap();
        src.apply_at(2, PluginOp::Install(&valid(ID_B, 'a', 2)))
            .unwrap();
        put_state(&src, 3, ID_A, 0, "n", b"1");
        put_state(&src, 4, ID_A, 1, "m", b"2");
        let snap = src.snapshot().unwrap();
        assert_eq!(snap.applied_index, Some(4));

        let (dst, _d2) = store();
        dst.apply_at(1, PluginOp::Install(&valid("00000000000000dd", 'd', 5)))
            .unwrap();
        put_state(&dst, 2, "00000000000000dd", 0, "x", b"1");
        dst.replace(&snap).unwrap();
        assert_eq!(dst.snapshot().unwrap(), snap);
        assert!(dst.get("00000000000000dd").unwrap().is_none());
        assert_eq!(
            dst.state("00000000000000dd").unwrap().0,
            0,
            "state outside the snapshot is gone"
        );
        assert_eq!(dst.state(ID_A).unwrap(), src.state(ID_A).unwrap());
        assert_eq!(dst.state(ID_A).unwrap().0, 2);
        assert_eq!(dst.applied_index().unwrap(), Some(4));
        // Entries the snapshot already covers are skipped after the install.
        assert_eq!(put_state(&dst, 4, ID_A, 2, "z", b"9"), Applied::Replayed);
    }

    #[test]
    fn a_snapshot_without_a_cursor_clears_it() {
        let (s, _d) = store();
        s.apply_at(7, PluginOp::Install(&valid(ID_A, 'a', 1)))
            .unwrap();
        s.replace(&PluginSnapshot::default()).unwrap();
        assert_eq!(s.applied_index().unwrap(), None);
    }

    #[test]
    fn snapshot_state_bytes_are_base64_not_number_arrays() {
        let snap = PluginSnapshot {
            installs: vec![],
            state: vec![StateSnap {
                id: ID_A.into(),
                rev: 1,
                entries: puts(&[("n", &[0, 1, 2, 255])]),
            }],
            secrets: vec![],
            applied_index: Some(1),
        };
        let json = serde_json::to_string(&snap).unwrap();
        assert!(json.contains("AAEC/w=="), "{json}");
        assert_eq!(serde_json::from_str::<PluginSnapshot>(&json).unwrap(), snap);
    }

    #[test]
    fn replacing_from_a_snapshot_leaves_blobs_alone() {
        let (s, _d) = store();
        s.put_blob("sha1", b"abc").unwrap();
        s.replace(&PluginSnapshot::default()).unwrap();
        assert_eq!(s.get_blob("sha1").unwrap().unwrap(), b"abc");
    }
    fn secret_record(id: &str) -> InstallRecord {
        let mut r = record(id, "sha1", 1);
        r.approved = Capabilities::parse(
            br#"{"log":true,"http":{"hosts":[{"host":"panel.example"}]},"secrets":[{"name":"PANEL_TOKEN","hosts":["panel.example"]}]}"#,
        )
        .unwrap();
        r
    }

    fn sealed(nonce_byte: u8) -> Sealed {
        use base64::Engine;
        let b = base64::engine::general_purpose::STANDARD;
        Sealed {
            key_id: "0123456789abcdef".into(),
            nonce: b.encode([nonce_byte; 24]),
            ciphertext: b.encode([9u8; 40]),
        }
    }

    #[test]
    fn a_rewrap_skips_a_slot_whose_ciphertext_changed_since_it_was_read() {
        let (s, _d) = store();
        s.put_blob("sha1", b"abc").unwrap();
        s.create(&secret_record(ID_A)).unwrap();
        let old = sealed(1);
        assert_eq!(
            s.put_secret(ID_A, "PANEL_TOKEN", &old, 1).unwrap(),
            Applied::Done(None)
        );
        let item = |from: &Sealed, to: Sealed| RewrapItem {
            id: ID_A.into(),
            slot: "PANEL_TOKEN".into(),
            from_nonce: from.nonce.clone(),
            sealed: to,
        };
        // An operator write lands between the rewrap's read and its apply.
        let newer = sealed(2);
        s.put_secret(ID_A, "PANEL_TOKEN", &newer, 2).unwrap();
        let skipped = s.rewrap(&[item(&old, sealed(3))], 3).unwrap();
        assert_eq!(skipped, vec![format!("{ID_A}/PANEL_TOKEN")]);
        assert_eq!(
            s.get_secret(ID_A, "PANEL_TOKEN").unwrap().unwrap().sealed,
            newer
        );
        // An unchanged slot is rewrapped.
        let fresh = sealed(4);
        assert!(s
            .rewrap(&[item(&newer, fresh.clone())], 4)
            .unwrap()
            .is_empty());
        assert_eq!(
            s.get_secret(ID_A, "PANEL_TOKEN").unwrap().unwrap().sealed,
            fresh
        );
    }

    #[test]
    fn secrets_need_an_approved_slot_and_survive_a_snapshot_round_trip() {
        let (s, _d) = store();
        s.put_blob("sha1", b"abc").unwrap();
        s.create(&secret_record(ID_A)).unwrap();
        assert!(matches!(
            s.put_secret(ID_A, "OTHER", &sealed(1), 1).unwrap(),
            Applied::Invalid(_)
        ));
        assert_eq!(
            s.put_secret("00000000000000bb", "PANEL_TOKEN", &sealed(1), 1)
                .unwrap(),
            Applied::NoSuchInstall
        );
        s.put_secret(ID_A, "PANEL_TOKEN", &sealed(1), 1).unwrap();
        let snap = s.snapshot().unwrap();
        assert_eq!(snap.secrets.len(), 1);
        let (t, _d2) = store();
        t.replace(&snap).unwrap();
        assert_eq!(
            t.get_secret(ID_A, "PANEL_TOKEN").unwrap().unwrap().sealed,
            sealed(1)
        );
        // Replacing from a snapshot without it clears the secret.
        t.replace(&PluginSnapshot::default()).unwrap();
        assert!(t.all_secrets().unwrap().is_empty());
    }

    #[test]
    fn deleting_an_install_wipes_its_secrets() {
        let (s, _d) = store();
        s.put_blob("sha1", b"abc").unwrap();
        s.create(&secret_record(ID_A)).unwrap();
        s.put_secret(ID_A, "PANEL_TOKEN", &sealed(1), 1).unwrap();
        s.delete(ID_A).unwrap();
        assert!(s.all_secrets().unwrap().is_empty());
    }
}
