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
//! holds the `current` pointer at the latest revision number and, for a
//! store driven by the Raft state machine, an `applied_index` — the highest
//! Raft log index whose effect this store has absorbed, written in the same
//! transaction as the revision so a replayed entry can be recognised and
//! skipped (see [`Store::put_applied`]).

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
    #[error("revision counter moved during a Raft apply (concurrent writer)")]
    ConcurrentWrite,
    #[error("a write without a Raft index was skipped as already applied")]
    UnexpectedSkip,
}

const CURRENT_KEY: &[u8] = b"current";
const APPLIED_INDEX_KEY: &[u8] = b"applied_index";

/// Outcome of [`Store::put_applied`] / [`Store::put_applied_with`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    /// The revision was written; this is its number.
    Written(u64),
    /// The store had already absorbed this log index (a replay) — nothing
    /// was written.
    AlreadyApplied,
}

/// One write to a sibling tree of the same database (e.g. the per-revision
/// stage or actor trees) that joins a [`Store::put_applied_with`]
/// transaction. `value: None` removes the key.
pub struct SiblingWrite<'a> {
    pub tree: &'a sled::Tree,
    pub key: Vec<u8>,
    pub value: Option<Vec<u8>>,
}

type TxError = sled::transaction::ConflictableTransactionError<TxAbort>;

/// Why a transaction body aborted itself (as opposed to a storage failure).
enum TxAbort {
    /// The `current` pointer moved between computing the next revision
    /// number and the transaction running — a concurrent non-Raft writer.
    RevisionRaced,
}

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

    /// Every revision, oldest first, with its number.
    pub fn all_revisions(&self) -> Result<Vec<(u64, RevisionBytes)>, StoreError> {
        self.revisions_after(0)
    }

    /// Drops every revision whose key (`key_of` of its bytes) a later
    /// revision repeats, keeping the newest per key; entries `key_of` gives
    /// no key are always kept. Returns how many were dropped.
    ///
    /// Revision numbers are untouched, so the log gains gaps but never
    /// renumbers: a subscriber's `since` cursor still yields every key that
    /// changed after it, at its newest revision. Only `revisions` changes,
    /// in one atomic batch, and the newest revision overall is always a
    /// key's newest, so the `current` pointer needs no update. Only a
    /// log whose entries are whole-key replacements may call this (the
    /// config and intent logs are history and never do).
    pub fn compact<K: Eq + std::hash::Hash>(
        &self,
        key_of: impl Fn(&[u8]) -> Option<K>,
    ) -> Result<usize, StoreError> {
        let mut newest: std::collections::HashMap<K, u64> = std::collections::HashMap::new();
        let mut dropped = Vec::new();
        for item in self.revisions.iter() {
            let (k, v) = item?;
            let revision = decode_rev(&k);
            if let Some(key) = key_of(&v) {
                if let Some(older) = newest.insert(key, revision) {
                    dropped.push(older);
                }
            }
        }
        if dropped.is_empty() {
            return Ok(0);
        }
        let mut batch = sled::Batch::default();
        for revision in &dropped {
            batch.remove(&encode_rev(*revision));
        }
        self.revisions.apply_batch(batch)?;
        self.db.flush()?;
        Ok(dropped.len())
    }

    /// Accepts a new revision: assigns the next monotonic number, persists
    /// the bytes and moves the `current` pointer in one `sled` transaction
    /// (across both trees), then flushes — a crash can lose the very last
    /// write, it can never leave a revision stored without becoming current
    /// or vice versa. Returns the assigned revision number.
    ///
    /// Callers (the slice-2 submit API) run `gsp_config::validate` *before*
    /// calling this — the store itself does not parse or validate `bytes`.
    #[allow(clippy::needless_pass_by_value)] // owns what it writes; callers hand the buffers over
    pub fn put(&self, bytes: RevisionBytes) -> Result<u64, StoreError> {
        self.put_with(bytes, &|_| Vec::new())
    }

    /// [`Store::put`] plus sibling-tree writes in the same transaction,
    /// without recording an `applied_index` — the non-HA write path of a
    /// registry whose `current` tree must move with its log. `siblings`
    /// receives the revision number being written; the trees must come from
    /// this store's own [`Store::db`] (see [`SiblingWrite`]).
    ///
    /// Overlapping puts race for the same revision number; the in-transaction
    /// `current` check lets exactly one win and the losers retry against the
    /// new `current`, so every put gets a distinct revision and none is lost.
    #[allow(clippy::needless_pass_by_value)] // owns what it writes; callers hand the buffers over
    pub fn put_with<'a>(
        &self,
        bytes: RevisionBytes,
        siblings: &dyn Fn(u64) -> Vec<SiblingWrite<'a>>,
    ) -> Result<u64, StoreError> {
        loop {
            let next = self.next_revision()?;
            match self.apply_at(None, Some((next, bytes.clone())), siblings(next)) {
                Ok(_) => return Ok(next),
                Err(StoreError::ConcurrentWrite) => continue,
                Err(e) => return Err(e),
            }
        }
    }

    /// The highest Raft log index this store has absorbed, or `None` if it
    /// has never been driven by the state machine.
    pub fn applied_index(&self) -> Result<Option<u64>, StoreError> {
        Ok(self.meta.get(APPLIED_INDEX_KEY)?.map(|v| decode_rev(&v)))
    }

    /// [`Store::put`] for a Raft apply step at log `index`: the revision
    /// and the `applied_index` land in one transaction, so a crash can
    /// never leave one without the other. Returns
    /// [`Applied::AlreadyApplied`], writing nothing, when the stored
    /// `applied_index` is already `>= index`.
    pub fn put_applied(&self, bytes: RevisionBytes, index: u64) -> Result<Applied, StoreError> {
        self.put_applied_with(bytes, index, &|_| Vec::new())
    }

    /// [`Store::put_applied`] plus writes to sibling trees of this same
    /// database, which join the transaction. `siblings` receives the
    /// revision number being written (stage/actor entries are keyed by it).
    pub fn put_applied_with<'a>(
        &self,
        bytes: RevisionBytes,
        index: u64,
        siblings: &dyn Fn(u64) -> Vec<SiblingWrite<'a>>,
    ) -> Result<Applied, StoreError> {
        let next = self.next_revision()?;
        let writes = siblings(next);
        Ok(
            match self.apply_at(Some(index), Some((next, bytes)), writes)? {
                true => Applied::Written(next),
                false => Applied::AlreadyApplied,
            },
        )
    }

    /// Advances `applied_index` to `index` for a step that writes no
    /// revision (every entry advances every database it could touch,
    /// whatever its outcome). Never moves the index backwards.
    pub fn mark_applied(&self, index: u64) -> Result<(), StoreError> {
        self.mark_applied_with(index, Vec::new()).map(|_| ())
    }

    /// [`Store::mark_applied`] plus sibling-tree writes in the same
    /// transaction (a stage flip, say). `Ok(false)` when `index` was already
    /// applied and nothing — siblings included — was written.
    pub fn mark_applied_with(
        &self,
        index: u64,
        siblings: Vec<SiblingWrite<'_>>,
    ) -> Result<bool, StoreError> {
        self.apply_at(Some(index), None, siblings)
    }

    fn next_revision(&self) -> Result<u64, StoreError> {
        match self.current_revision()? {
            Some(rev) => rev.checked_add(1).ok_or(StoreError::CounterOverflow),
            None => Ok(1),
        }
    }

    /// The shared transaction: skip if already applied, else write the
    /// optional revision, the `applied_index` and the sibling writes.
    /// `true` = written, `false` = skipped. `index: None` (a non-Raft
    /// write) never skips and leaves `applied_index` untouched.
    #[allow(clippy::needless_pass_by_value)] // owns what it writes; callers hand the buffers over
    fn apply_at(
        &self,
        index: Option<u64>,
        revision: Option<(u64, RevisionBytes)>,
        siblings: Vec<SiblingWrite<'_>>,
    ) -> Result<bool, StoreError> {
        let (trees, slots) = self.transaction_trees(&siblings);
        let outcome = trees[..].transaction(|views| {
            let (revisions, meta) = (&views[0], &views[1]);
            if let (Some(index), Some(done)) = (index, meta.get(APPLIED_INDEX_KEY)?) {
                if decode_rev(&done) >= index {
                    return Ok::<_, TxError>(false);
                }
            }
            if let Some((next, bytes)) = &revision {
                let expected = meta.get(CURRENT_KEY)?.map(|v| decode_rev(&v));
                if expected.map_or(1, |r| r.wrapping_add(1)) != *next {
                    return Err(TxError::Abort(TxAbort::RevisionRaced));
                }
                revisions.insert(&encode_rev(*next), bytes.as_slice())?;
                meta.insert(CURRENT_KEY, &encode_rev(*next))?;
            }
            if let Some(index) = index {
                meta.insert(APPLIED_INDEX_KEY, &encode_rev(index))?;
            }
            for (w, slot) in siblings.iter().zip(&slots) {
                match &w.value {
                    Some(v) => views[*slot].insert(w.key.as_slice(), v.as_slice())?,
                    None => views[*slot].remove(w.key.as_slice())?,
                };
            }
            Ok(true)
        });
        let written = outcome.map_err(|e| match e {
            sled::transaction::TransactionError::Storage(e) => StoreError::Sled(e),
            sled::transaction::TransactionError::Abort(TxAbort::RevisionRaced) => {
                StoreError::ConcurrentWrite
            }
        })?;
        if written {
            self.db.flush()?;
        }
        Ok(written)
    }

    /// Replaces the whole store in one transaction — a Raft snapshot
    /// install: every existing revision is removed, `revisions` written at
    /// their given numbers (which need not start at 1), `current` set to
    /// the highest of them (removed when there are none) and
    /// `applied_index` set to `applied_index` (removed when `None`).
    pub fn replace_all(
        &self,
        revisions: &[(u64, RevisionBytes)],
        applied_index: Option<u64>,
    ) -> Result<(), StoreError> {
        self.replace_all_with(revisions, applied_index, &[], Vec::new())
    }

    /// [`Store::replace_all`] that also empties every tree in `clear` and
    /// then applies `siblings`, all in the same transaction (the config
    /// store's stage and actor trees). Like [`Store::put_applied_with`]'s
    /// siblings, every tree must come from this store's own [`Store::db`]:
    /// trees are matched to the transaction by name only.
    ///
    /// `sled` transactions cannot iterate, so the keys to remove are read
    /// just before the transaction; the caller must be the only writer
    /// (the Raft state-machine worker, which serializes install with
    /// apply).
    pub fn replace_all_with<'a>(
        &self,
        revisions: &[(u64, RevisionBytes)],
        applied_index: Option<u64>,
        clear: &[&'a sled::Tree],
        siblings: Vec<SiblingWrite<'a>>,
    ) -> Result<(), StoreError> {
        let stale = self
            .revisions
            .iter()
            .keys()
            .collect::<Result<Vec<_>, _>>()?;
        let mut writes = Vec::new();
        for tree in clear {
            for key in tree.iter().keys() {
                writes.push(SiblingWrite {
                    tree,
                    key: key?.to_vec(),
                    value: None,
                });
            }
        }
        writes.extend(siblings);
        let current = revisions.iter().map(|(r, _)| *r).max();

        let (trees, slots) = self.transaction_trees(&writes);
        trees[..]
            .transaction(|views| {
                let (revs, meta) = (&views[0], &views[1]);
                for key in &stale {
                    revs.remove(key)?;
                }
                for (revision, bytes) in revisions {
                    revs.insert(&encode_rev(*revision), bytes.as_slice())?;
                }
                match current {
                    Some(r) => meta.insert(CURRENT_KEY, &encode_rev(r))?,
                    None => meta.remove(CURRENT_KEY)?,
                };
                match applied_index {
                    Some(i) => meta.insert(APPLIED_INDEX_KEY, &encode_rev(i))?,
                    None => meta.remove(APPLIED_INDEX_KEY)?,
                };
                for (w, slot) in writes.iter().zip(&slots) {
                    match &w.value {
                        Some(v) => views[*slot].insert(w.key.as_slice(), v.as_slice())?,
                        None => views[*slot].remove(w.key.as_slice())?,
                    };
                }
                Ok::<_, TxError>(())
            })
            .map_err(|e| match e {
                sled::transaction::TransactionError::Storage(e) => StoreError::Sled(e),
                sled::transaction::TransactionError::Abort(TxAbort::RevisionRaced) => {
                    StoreError::ConcurrentWrite
                }
            })?;
        self.db.flush()?;
        Ok(())
    }

    /// The trees a transaction over `revisions`, `meta` and `writes` spans:
    /// trees 0 and 1 are `revisions` and `meta`, each distinct sibling tree
    /// follows, and `slots[i]` names the position of `writes[i]`'s tree.
    fn transaction_trees<'t>(
        &'t self,
        writes: &[SiblingWrite<'t>],
    ) -> (Vec<&'t sled::Tree>, Vec<usize>) {
        let mut trees: Vec<&sled::Tree> = vec![&self.revisions, &self.meta];
        let mut slots = Vec::with_capacity(writes.len());
        for w in writes {
            let slot = match trees.iter().position(|t| t.name() == w.tree.name()) {
                Some(slot) => slot,
                None => {
                    trees.push(w.tree);
                    trees.len() - 1
                }
            };
            slots.push(slot);
        }
        (trees, slots)
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

/// Runs `open` until it stops failing on `sled`'s exclusive file lock.
///
/// Dropping the last `sled::Db` handle does not release the lock
/// synchronously: the locked `File` sits behind an `Arc` that `sled` 0.34's
/// own threadpool jobs (async log writes, segment truncation) and
/// epoch-deferred buffer drops also hold, so it is closed whenever the last
/// of those finishes. Opening a path again right after dropping it in the
/// same process (a test fixture, or the pre-HA import, which inspects a
/// registry, renames it and reads it again) can lose that race with
/// `WouldBlock` ("could not acquire lock"). Only that error is retried, and
/// only for a bounded time; any other outcome, `Ok` or `Err`, is returned, as
/// is the lock error itself once the time is up (a second process really
/// holding the lock).
pub(crate) fn retry_when_unlocked<T, E: std::fmt::Display>(
    mut open: impl FnMut() -> Result<T, E>,
) -> Result<T, E> {
    use std::time::{Duration, Instant};

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match open() {
            // `{e:#}` so a lock error under `anyhow` context still matches.
            Err(e)
                if format!("{e:#}").contains("could not acquire lock")
                    && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            other => return other,
        }
    }
}

/// [`retry_when_unlocked`] for an open that must succeed.
#[cfg(test)]
pub(crate) fn reopen_when_unlocked<T, E: std::fmt::Display>(
    open: impl FnMut() -> Result<T, E>,
) -> T {
    match retry_when_unlocked(open) {
        Ok(v) => v,
        Err(e) => panic!("reopen failed: {e:#}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reopen_retries_a_lock_error_wrapped_in_context() {
        // `anyhow` context hides the cause from plain `Display`; the helper
        // must still recognise sled's lock error underneath it.
        let mut calls = 0;
        let v = reopen_when_unlocked(|| {
            calls += 1;
            if calls < 3 {
                Err(anyhow::anyhow!("could not acquire lock").context("opening \"peers\""))
            } else {
                Ok(calls)
            }
        });
        assert_eq!(v, 3);
    }

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

    const N: usize = 16;

    #[test]
    fn concurrent_puts_each_get_a_distinct_revision() {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(Store::open(dir.path()).unwrap());
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(N));
        let handles: Vec<_> = (0..N)
            .map(|i| {
                let store = store.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    store.put(format!("config: {i}").into_bytes()).unwrap()
                })
            })
            .collect();
        let mut revs: Vec<u64> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        revs.sort_unstable();
        assert_eq!(revs, (1..=N as u64).collect::<Vec<_>>());
        assert_eq!(store.revisions_after(0).unwrap().len(), N);
        assert_eq!(store.current_revision().unwrap(), Some(N as u64));
    }

    #[test]
    fn concurrent_put_with_keeps_side_entries_on_their_own_revision() {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(Store::open(dir.path()).unwrap());
        let side = store.db().open_tree("side").unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(N));
        let handles: Vec<_> = (0..N)
            .map(|i| {
                let (store, side, barrier) = (store.clone(), side.clone(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    let tag = format!("tag-{i}").into_bytes();
                    let rev = store
                        .put_with(format!("config: {i}").into_bytes(), &|revision| {
                            vec![SiblingWrite {
                                tree: &side,
                                key: encode_rev(revision).to_vec(),
                                value: Some(tag.clone()),
                            }]
                        })
                        .unwrap();
                    (rev, i)
                })
            })
            .collect();
        let mut revs = Vec::new();
        for h in handles {
            let (rev, i) = h.join().unwrap();
            assert_eq!(
                store.get(rev).unwrap().unwrap(),
                format!("config: {i}").into_bytes()
            );
            assert_eq!(
                side.get(encode_rev(rev)).unwrap().unwrap().to_vec(),
                format!("tag-{i}").into_bytes()
            );
            revs.push(rev);
        }
        revs.sort_unstable();
        assert_eq!(revs, (1..=N as u64).collect::<Vec<_>>());
    }

    #[test]
    fn current_pointer_survives_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let rev = {
            let store = Store::open(dir.path()).unwrap();
            store.put(b"config: first".to_vec()).unwrap();
            store.put(b"config: second".to_vec()).unwrap()
        };

        let reopened = reopen_when_unlocked(|| Store::open(dir.path()));
        assert_eq!(reopened.current_revision().unwrap(), Some(rev));
        assert_eq!(reopened.get(rev).unwrap().unwrap(), b"config: second");
    }

    #[test]
    fn put_applied_skips_an_index_it_has_already_seen() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.applied_index().unwrap(), None);

        assert!(matches!(
            store.put_applied(b"one".to_vec(), 7).unwrap(),
            Applied::Written(1)
        ));
        assert_eq!(store.applied_index().unwrap(), Some(7));
        // Same index again, and any lower one: skipped, nothing written.
        assert!(matches!(
            store.put_applied(b"dup".to_vec(), 7).unwrap(),
            Applied::AlreadyApplied
        ));
        assert!(matches!(
            store.put_applied(b"old".to_vec(), 3).unwrap(),
            Applied::AlreadyApplied
        ));
        assert_eq!(store.current_revision().unwrap(), Some(1));
        assert!(matches!(
            store.put_applied(b"two".to_vec(), 8).unwrap(),
            Applied::Written(2)
        ));
    }

    #[test]
    fn compact_keeps_the_newest_revision_per_key_and_every_number() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        for entry in ["a:1", "b:1", "a:2", "opaque", "a:3", "b:2"] {
            store.put(entry.as_bytes().to_vec()).unwrap();
        }
        let key_of = |bytes: &[u8]| {
            std::str::from_utf8(bytes)
                .ok()
                .and_then(|s| s.split_once(':'))
                .map(|(k, _)| k.to_string())
        };
        assert_eq!(store.compact(key_of).unwrap(), 3);
        // Revisions keep their numbers; an entry with no key is never dropped.
        let kept: Vec<u64> = store
            .all_revisions()
            .unwrap()
            .into_iter()
            .map(|(r, _)| r)
            .collect();
        assert_eq!(kept, vec![4, 5, 6]);
        assert_eq!(store.current_revision().unwrap(), Some(6));
        // Nothing left to drop, and the numbering carries on.
        assert_eq!(store.compact(key_of).unwrap(), 0);
        assert_eq!(store.put(b"c:1".to_vec()).unwrap(), 7);
    }

    #[test]
    fn mark_applied_advances_the_index_without_a_revision() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.mark_applied(4).unwrap();
        assert_eq!(store.applied_index().unwrap(), Some(4));
        assert_eq!(store.current_revision().unwrap(), None);
        // Never moves backwards.
        store.mark_applied(2).unwrap();
        assert_eq!(store.applied_index().unwrap(), Some(4));
    }

    #[test]
    fn put_applied_with_writes_siblings_in_the_same_transaction() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let side = store.db().open_tree("side").unwrap();

        let applied = store
            .put_applied_with(b"one".to_vec(), 1, &|rev| {
                vec![SiblingWrite {
                    tree: &side,
                    key: rev.to_be_bytes().to_vec(),
                    value: Some(b"v".to_vec()),
                }]
            })
            .unwrap();
        assert!(matches!(applied, Applied::Written(1)));
        assert_eq!(
            side.get(1u64.to_be_bytes()).unwrap().unwrap().as_ref(),
            b"v"
        );

        // A skipped step leaves its siblings alone too.
        let applied = store
            .put_applied_with(b"dup".to_vec(), 1, &|rev| {
                vec![SiblingWrite {
                    tree: &side,
                    key: rev.to_be_bytes().to_vec(),
                    value: None,
                }]
            })
            .unwrap();
        assert!(matches!(applied, Applied::AlreadyApplied));
        assert!(side.get(1u64.to_be_bytes()).unwrap().is_some());
    }

    #[test]
    fn put_with_writes_siblings_but_records_no_applied_index() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let side = store.db().open_tree("side").unwrap();
        let siblings = |rev: u64| {
            vec![SiblingWrite {
                tree: &side,
                key: b"k".to_vec(),
                value: Some(rev.to_be_bytes().to_vec()),
            }]
        };

        assert_eq!(store.put_with(b"one".to_vec(), &siblings).unwrap(), 1);
        assert_eq!(store.put_with(b"two".to_vec(), &siblings).unwrap(), 2);
        assert_eq!(store.current_revision().unwrap(), Some(2));
        assert_eq!(
            side.get(b"k").unwrap().unwrap().as_ref(),
            2u64.to_be_bytes()
        );
        assert_eq!(store.applied_index().unwrap(), None);
    }

    #[test]
    fn replace_all_with_clears_and_writes_at_the_given_numbers() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let side = store.db().open_tree("side").unwrap();
        for i in 1..=3 {
            store.put_applied(vec![i as u8], i).unwrap();
            side.insert(i.to_be_bytes(), b"old".to_vec()).unwrap();
        }

        let revisions = vec![(5, b"five".to_vec()), (6, b"six".to_vec())];
        let writes = vec![SiblingWrite {
            tree: &side,
            key: 6u64.to_be_bytes().to_vec(),
            value: Some(b"new".to_vec()),
        }];
        store
            .replace_all_with(&revisions, Some(9), &[&side], writes)
            .unwrap();

        assert_eq!(store.all_revisions().unwrap(), revisions);
        assert_eq!(store.current_revision().unwrap(), Some(6));
        assert_eq!(store.applied_index().unwrap(), Some(9));
        let side_keys: Vec<_> = side.iter().keys().map(|k| k.unwrap().to_vec()).collect();
        assert_eq!(side_keys, vec![6u64.to_be_bytes().to_vec()]);

        // An empty snapshot empties the store and forgets the index.
        store.replace_all(&[], None).unwrap();
        assert!(store.all_revisions().unwrap().is_empty());
        assert_eq!(store.current_revision().unwrap(), None);
        assert_eq!(store.applied_index().unwrap(), None);
    }
}
