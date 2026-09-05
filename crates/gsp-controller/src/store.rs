//! Tier-1 revision store: an embedded `sled` KV, single node (ADR 20,
//! `docs/09-technology-choices.md`). `sled` gives crash-consistent writes and
//! atomic transactions without standing up a separate service — appropriate
//! for this release's single `standalone` controller (`docs/10` "Fleet
//! topology"). Swapping the backing store for a distributed one later is a
//! change behind this same API, not a rewrite of its callers.
//!
//! Two `sled` trees: `revisions` maps a big-endian `u64` revision number to
//! the raw config bytes accepted at that revision (whatever `gsp_config`
//! would parse, byte-for-byte — this crate never interprets them); `meta`
//! holds a single `current` pointer at the latest revision number.

use std::path::Path;

use sled::Transactional;
use thiserror::Error;

/// The exact bytes submitted for one revision (a YAML document today; the
/// store itself is content-agnostic).
pub type RevisionBytes = Vec<u8>;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("store I/O error: {0}")]
    Sled(#[from] sled::Error),
    #[error("revision counter exhausted u64")]
    CounterOverflow,
}

const CURRENT_KEY: &[u8] = b"current";

pub struct Store {
    db: sled::Db,
    revisions: sled::Tree,
    meta: sled::Tree,
}

impl Store {
    /// Opens (or creates) the store at `path`.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let db = sled::open(path)?;
        let revisions = db.open_tree("revisions")?;
        let meta = db.open_tree("meta")?;
        Ok(Store {
            db,
            revisions,
            meta,
        })
    }

    /// The underlying `sled` database — lets a caller (`crate::api::Stage`,
    /// phase 12 slice 7) open its own sibling tree in the exact same
    /// database rather than tracking a second `sled::open` path. `Store`
    /// itself stays content-agnostic; this is only ever used to add trees
    /// *alongside* `revisions`/`meta`, never to touch them directly.
    pub fn db(&self) -> &sled::Db {
        &self.db
    }

    /// The latest accepted revision number, or `None` before the first
    /// submission ever lands.
    pub fn current_revision(&self) -> Result<Option<u64>, StoreError> {
        Ok(self.meta.get(CURRENT_KEY)?.map(|v| decode_rev(&v)))
    }

    /// The raw bytes stored at `revision`, if it exists.
    pub fn get(&self, revision: u64) -> Result<Option<RevisionBytes>, StoreError> {
        Ok(self
            .revisions
            .get(encode_rev(revision))?
            .map(|v| v.to_vec()))
    }

    /// The latest revision's number and bytes, if any submission has landed.
    pub fn current(&self) -> Result<Option<(u64, RevisionBytes)>, StoreError> {
        let Some(rev) = self.current_revision()? else {
            return Ok(None);
        };
        Ok(self.get(rev)?.map(|bytes| (rev, bytes)))
    }

    /// Every revision strictly after `since`, oldest first — the catch-up
    /// range a subscriber replays before switching to the live tail. `sled`
    /// keys sort by byte order, and `encode_rev` is big-endian, so a plain
    /// range scan is already revision order.
    pub fn revisions_after(&self, since: u64) -> Result<Vec<(u64, RevisionBytes)>, StoreError> {
        use std::ops::Bound;

        let mut out = Vec::new();
        let range = (Bound::Excluded(encode_rev(since)), Bound::Unbounded);
        for item in self.revisions.range::<[u8; 8], _>(range) {
            let (k, v) = item?;
            out.push((decode_rev(&k), v.to_vec()));
        }
        Ok(out)
    }

    /// Accepts a new revision: assigns the next monotonic number, persists
    /// the bytes and moves the `current` pointer in one `sled` transaction
    /// (across both trees), then flushes — a crash can lose the very last
    /// write, it can never leave a revision stored without becoming current
    /// or vice versa. Returns the assigned revision number.
    ///
    /// Callers (the slice-2 submit API) run `gsp_config::validate` *before*
    /// calling this — the store itself does not parse or validate `bytes`.
    pub fn put(&self, bytes: RevisionBytes) -> Result<u64, StoreError> {
        let next = match self.current_revision()? {
            Some(rev) => rev.checked_add(1).ok_or(StoreError::CounterOverflow)?,
            None => 1,
        };

        (&self.revisions, &self.meta)
            .transaction(|(revisions, meta)| {
                revisions.insert(&encode_rev(next), bytes.as_slice())?;
                meta.insert(CURRENT_KEY, &encode_rev(next))?;
                Ok::<_, sled::transaction::ConflictableTransactionError<sled::Error>>(())
            })
            .map_err(|e| match e {
                sled::transaction::TransactionError::Storage(e) => StoreError::Sled(e),
                sled::transaction::TransactionError::Abort(e) => StoreError::Sled(e),
            })?;

        self.revisions.flush()?;
        self.meta.flush()?;
        Ok(next)
    }
}

fn encode_rev(rev: u64) -> [u8; 8] {
    rev.to_be_bytes()
}

fn decode_rev(bytes: &[u8]) -> u64 {
    let mut buf = [0u8; 8];
    buf.copy_from_slice(bytes);
    u64::from_be_bytes(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_store_has_no_current_revision() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.current_revision().unwrap(), None);
        assert!(store.current().unwrap().is_none());
    }

    #[test]
    fn revisions_are_monotonic_and_retrievable() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();

        let rev1 = store.put(b"config: one".to_vec()).unwrap();
        let rev2 = store.put(b"config: two".to_vec()).unwrap();
        assert!(rev2 > rev1);

        assert_eq!(store.get(rev1).unwrap().unwrap(), b"config: one");
        assert_eq!(store.get(rev2).unwrap().unwrap(), b"config: two");
        assert_eq!(
            store.current().unwrap().unwrap(),
            (rev2, b"config: two".to_vec())
        );
    }

    #[test]
    fn a_missing_revision_is_none_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.put(b"config: one".to_vec()).unwrap();
        assert!(store.get(9999).unwrap().is_none());
    }

    #[test]
    fn revisions_after_returns_the_catch_up_range_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let rev1 = store.put(b"one".to_vec()).unwrap();
        let rev2 = store.put(b"two".to_vec()).unwrap();
        let rev3 = store.put(b"three".to_vec()).unwrap();

        let all = store.revisions_after(0).unwrap();
        assert_eq!(
            all,
            vec![
                (rev1, b"one".to_vec()),
                (rev2, b"two".to_vec()),
                (rev3, b"three".to_vec())
            ]
        );

        let tail = store.revisions_after(rev1).unwrap();
        assert_eq!(
            tail,
            vec![(rev2, b"two".to_vec()), (rev3, b"three".to_vec())]
        );

        assert!(store.revisions_after(rev3).unwrap().is_empty());
    }

    #[test]
    fn current_pointer_survives_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let rev = {
            let store = Store::open(dir.path()).unwrap();
            store.put(b"config: first".to_vec()).unwrap();
            store.put(b"config: second".to_vec()).unwrap()
        };

        let reopened = Store::open(dir.path()).unwrap();
        assert_eq!(reopened.current_revision().unwrap(), Some(rev));
        assert_eq!(reopened.get(rev).unwrap().unwrap(), b"config: second");
    }
}
