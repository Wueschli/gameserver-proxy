//! `RaftLogStorage` + `RaftLogReader`: the replicated Raft log itself
//! (distinct from the config/intent revision logs it carries — see
//! `crate::ha` module doc). Adapted from `openraft`'s own
//! `examples/memstore` reference implementation, with one real change: that
//! example keeps the log in a `BTreeMap` (lost on restart, fine for a demo);
//! this one persists every entry, the vote, and the purge point to two
//! `sled` trees so a replica's log survives a process restart, not just its
//! applied state machine.

use std::fmt::Debug;
use std::ops::{Bound, RangeBounds};

use openraft::storage::{LogFlushed, RaftLogReader, RaftLogStorage};
use openraft::{LogId, LogState, RaftLogId, StorageError, StorageIOError, Vote};

use super::TypeConfig;

const VOTE_KEY: &[u8] = b"vote";
const LAST_PURGED_KEY: &[u8] = b"last_purged";

fn encode_idx(idx: u64) -> [u8; 8] {
    idx.to_be_bytes()
}

/// `sled`-backed Raft log. `Clone` (cheap — a `sled::Tree` is an `Arc`
/// internally) rather than the reference implementation's
/// `Arc<Mutex<BTreeMap<..>>>`: every method reads/writes `sled` directly, so
/// there is no separate in-process cache to keep consistent with the
/// on-disk state, and `sled` itself already serializes writes per tree.
#[derive(Clone)]
pub struct LogStore {
    entries: sled::Tree,
    meta: sled::Tree,
}

impl LogStore {
    pub fn open(db: &sled::Db) -> Result<Self, sled::Error> {
        Ok(LogStore {
            entries: db.open_tree("raft_log")?,
            meta: db.open_tree("raft_log_meta")?,
        })
    }

    fn io_err(e: impl std::error::Error + 'static) -> StorageError<NodeIdT> {
        StorageIOError::write(&e).into()
    }
}

type NodeIdT = super::NodeId;

impl RaftLogReader<TypeConfig> for LogStore {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + Send>(
        &mut self,
        range: RB,
    ) -> Result<Vec<openraft::Entry<TypeConfig>>, StorageError<NodeIdT>> {
        let start = match range.start_bound() {
            Bound::Included(&n) => Bound::Included(encode_idx(n)),
            Bound::Excluded(&n) => Bound::Excluded(encode_idx(n)),
            Bound::Unbounded => Bound::Unbounded,
        };
        let end = match range.end_bound() {
            Bound::Included(&n) => Bound::Included(encode_idx(n)),
            Bound::Excluded(&n) => Bound::Excluded(encode_idx(n)),
            Bound::Unbounded => Bound::Unbounded,
        };
        let mut out = Vec::new();
        for item in self.entries.range::<[u8; 8], _>((start, end)) {
            let (_, v) = item.map_err(Self::io_err)?;
            let entry: openraft::Entry<TypeConfig> =
                serde_json::from_slice(&v).map_err(Self::io_err)?;
            out.push(entry);
        }
        Ok(out)
    }
}

impl RaftLogStorage<TypeConfig> for LogStore {
    type LogReader = Self;

    async fn get_log_state(&mut self) -> Result<LogState<TypeConfig>, StorageError<NodeIdT>> {
        let last_purged_log_id = self
            .meta
            .get(LAST_PURGED_KEY)
            .map_err(Self::io_err)?
            .map(|v| serde_json::from_slice::<LogId<NodeIdT>>(&v))
            .transpose()
            .map_err(Self::io_err)?;

        let last = self.entries.last().map_err(Self::io_err)?;
        let last_log_id = match last {
            Some((_, v)) => {
                let entry: openraft::Entry<TypeConfig> =
                    serde_json::from_slice(&v).map_err(Self::io_err)?;
                Some(*entry.get_log_id())
            }
            None => last_purged_log_id,
        };

        Ok(LogState {
            last_purged_log_id,
            last_log_id,
        })
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        self.clone()
    }

    async fn save_vote(&mut self, vote: &Vote<NodeIdT>) -> Result<(), StorageError<NodeIdT>> {
        let bytes = serde_json::to_vec(vote).map_err(Self::io_err)?;
        self.meta.insert(VOTE_KEY, bytes).map_err(Self::io_err)?;
        self.meta.flush_async().await.map_err(Self::io_err)?;
        Ok(())
    }

    async fn read_vote(&mut self) -> Result<Option<Vote<NodeIdT>>, StorageError<NodeIdT>> {
        self.meta
            .get(VOTE_KEY)
            .map_err(Self::io_err)?
            .map(|v| serde_json::from_slice(&v))
            .transpose()
            .map_err(Self::io_err)
    }

    async fn append<I>(
        &mut self,
        entries: I,
        callback: LogFlushed<TypeConfig>,
    ) -> Result<(), StorageError<NodeIdT>>
    where
        I: IntoIterator<Item = openraft::Entry<TypeConfig>> + Send,
        I::IntoIter: Send,
    {
        for entry in entries {
            let idx = entry.get_log_id().index;
            let bytes = serde_json::to_vec(&entry).map_err(Self::io_err)?;
            self.entries
                .insert(encode_idx(idx), bytes)
                .map_err(Self::io_err)?;
        }
        self.entries.flush_async().await.map_err(Self::io_err)?;
        callback.log_io_completed(Ok(()));
        Ok(())
    }

    async fn truncate(&mut self, log_id: LogId<NodeIdT>) -> Result<(), StorageError<NodeIdT>> {
        let keys: Vec<[u8; 8]> = self
            .entries
            .range::<[u8; 8], _>((Bound::Included(encode_idx(log_id.index)), Bound::Unbounded))
            .map(|item| item.map(|(k, _)| k))
            .collect::<Result<Vec<_>, _>>()
            .map_err(Self::io_err)?
            .into_iter()
            .map(|k| {
                let mut buf = [0u8; 8];
                buf.copy_from_slice(&k);
                buf
            })
            .collect();
        for k in keys {
            self.entries.remove(k).map_err(Self::io_err)?;
        }
        self.entries.flush_async().await.map_err(Self::io_err)?;
        Ok(())
    }

    async fn purge(&mut self, log_id: LogId<NodeIdT>) -> Result<(), StorageError<NodeIdT>> {
        let keys: Vec<[u8; 8]> = self
            .entries
            .range::<[u8; 8], _>((Bound::Unbounded, Bound::Included(encode_idx(log_id.index))))
            .map(|item| item.map(|(k, _)| k))
            .collect::<Result<Vec<_>, _>>()
            .map_err(Self::io_err)?
            .into_iter()
            .map(|k| {
                let mut buf = [0u8; 8];
                buf.copy_from_slice(&k);
                buf
            })
            .collect();
        for k in keys {
            self.entries.remove(k).map_err(Self::io_err)?;
        }
        let bytes = serde_json::to_vec(&log_id).map_err(Self::io_err)?;
        self.meta
            .insert(LAST_PURGED_KEY, bytes)
            .map_err(Self::io_err)?;
        self.entries.flush_async().await.map_err(Self::io_err)?;
        self.meta.flush_async().await.map_err(Self::io_err)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openraft::{CommittedLeaderId, Entry, EntryPayload, LogId};

    fn open() -> (LogStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = sled::open(dir.path()).unwrap();
        (LogStore::open(&db).unwrap(), dir)
    }

    fn entry(index: u64) -> openraft::Entry<TypeConfig> {
        Entry {
            log_id: LogId::new(CommittedLeaderId::new(1, 0), index),
            payload: EntryPayload::Blank,
        }
    }

    /// `openraft::storage::LogFlushed::new` is crate-private upstream (only
    /// `openraft`'s own core ever constructs one to hand to `append`), so
    /// these tests seed entries straight into the backing tree instead of
    /// going through the trait method — exercising the same encode/decode
    /// and range-query logic `append`/`try_get_log_entries` share, without
    /// needing a real callback. `append` itself gets real exercise from
    /// `crates/gsp-fleet-tests`' multi-process HA test, which drives an
    /// actual `Raft` end to end.
    fn seed(store: &LogStore, entries: &[openraft::Entry<TypeConfig>]) {
        for e in entries {
            let bytes = serde_json::to_vec(e).unwrap();
            store
                .entries
                .insert(encode_idx(e.get_log_id().index), bytes)
                .unwrap();
        }
    }

    /// sled releases its file lock on a background thread after the last
    /// handle drops, so an immediate reopen can lose the race under load.
    fn reopen(path: &std::path::Path) -> sled::Db {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match sled::open(path) {
                Ok(db) => return db,
                Err(e) if std::time::Instant::now() >= deadline => {
                    panic!("sled never released its lock: {e}")
                }
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(20)),
            }
        }
    }

    #[tokio::test]
    async fn seeded_entries_are_readable_and_persist_across_a_reopen() {
        let (store, dir) = open();
        seed(&store, &[entry(1), entry(2)]);

        let mut reader = store.clone();
        let read = reader.try_get_log_entries(1..=2).await.unwrap();
        assert_eq!(read.len(), 2);

        drop(store);
        drop(reader); // both clones must go — sled holds one file lock per open Db
        let db = reopen(dir.path());
        let mut reopened = LogStore::open(&db).unwrap();
        let state = reopened.get_log_state().await.unwrap();
        assert_eq!(state.last_log_id.unwrap().index, 2);
    }

    #[tokio::test]
    async fn vote_round_trips() {
        let (mut store, _dir) = open();
        assert!(store.read_vote().await.unwrap().is_none());
        let vote = Vote::new(3, 7);
        store.save_vote(&vote).await.unwrap();
        assert_eq!(store.read_vote().await.unwrap(), Some(vote));
    }

    #[tokio::test]
    async fn purge_removes_entries_up_to_and_including_the_given_id() {
        let (mut store, _dir) = open();
        seed(&store, &[entry(1), entry(2), entry(3)]);

        store.purge(*entry(2).get_log_id()).await.unwrap();
        let remaining = store.try_get_log_entries(..).await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].get_log_id().index, 3);

        let state = store.get_log_state().await.unwrap();
        assert_eq!(state.last_purged_log_id.unwrap().index, 2);
    }
}
