//! `RaftStateMachine` + `RaftSnapshotBuilder`: applying a committed
//! [`super::WriteRequest`] means calling the index-aware
//! [`crate::api::AppState::apply_entry`] /
//! [`crate::intent::api::IntentState::apply_entry`] — the same writes a
//! direct (non-HA) call makes, plus the Raft log index recorded in the
//! same `sled` transaction so a replayed entry is skipped (crash-idempotent
//! apply) — see `crate::ha`'s module doc. `sled` (via those two
//! `Store`s) is the actual durable content; this module only adds the
//! bookkeeping `openraft` needs on top: last-applied log id, membership,
//! and snapshots.
//!
//! Snapshots are a main path, not a corner case: `openraft` purges the log
//! per [`super::raft_config`]'s snapshot policy, and a follower or learner
//! that lags behind the purge point is caught up by snapshot. A snapshot
//! carries every config and intent revision *at its number* (with the
//! config revision's stage and actor) and each store's `applied_index`;
//! installing one **replaces** both stores. `get_snapshot_builder` copies
//! that state while serialized with `apply` (the builder then runs in
//! parallel with later applies and never reads live state), and the last
//! built or installed snapshot is persisted in the `raft_snapshot` tree so
//! `get_current_snapshot` can serve it.

use std::sync::Arc;

use openraft::storage::{RaftStateMachine, Snapshot};
use openraft::{
    Entry, EntryPayload, LogId, RaftSnapshotBuilder, SnapshotMeta, StorageError, StorageIOError,
    StoredMembership,
};
use serde::{Deserialize, Serialize};
use sled::transaction::{ConflictableTransactionError, TransactionError};

use super::{NodeId, TypeConfig, WriteRequest, WriteResponse};
use crate::api::{AppState, Stage};
use crate::intent::api::IntentState;
use crate::store::SiblingWrite;

const SM_META_KEY: &[u8] = b"sm_meta";
/// Keys of the `raft_snapshot` tree: the current snapshot's
/// [`SnapshotMeta`] (JSON) and its serialized [`SnapshotContent`].
const SNAPSHOT_META_KEY: &[u8] = b"meta";
const SNAPSHOT_DATA_KEY: &[u8] = b"data";

type Membership = StoredMembership<NodeId, openraft::BasicNode>;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SmMeta {
    last_applied_log: Option<LogId<NodeId>>,
    last_membership: Membership,
}

/// One config revision in a snapshot, with the metadata stored beside it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RevisionSnap {
    pub revision: u64,
    pub bytes: Vec<u8>,
    pub stage: Stage,
    pub actor: Option<String>,
}

/// The full snapshot content: every revision of both logs at its number,
/// and each store's `applied_index` — enough to replace a replica's
/// config and intent `Store`s exactly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotContent {
    pub config: Vec<RevisionSnap>,
    pub intent: Vec<(u64, Vec<u8>)>,
    pub config_applied: Option<u64>,
    pub intent_applied: Option<u64>,
}

/// An owned copy of everything a snapshot holds, taken by
/// `get_snapshot_builder` on the state-machine worker (serialized with
/// `apply`), so building it later never reads live state.
pub struct SnapshotCopy {
    content: SnapshotContent,
    meta: SmMeta,
}

/// `openraft`'s snapshot builder: serializes its [`SnapshotCopy`] and
/// persists the result as the current snapshot. `copy` is the error when
/// taking the copy failed (`get_snapshot_builder` cannot return one);
/// `build_snapshot` reports it.
pub struct SnapshotBuilder {
    copy: Result<SnapshotCopy, StorageError<NodeId>>,
    snapshots: sled::Tree,
}

pub struct StateMachineStore {
    config: Arc<AppState>,
    intent: Arc<IntentState>,
    meta: sled::Tree,
    /// The last built or installed snapshot (`meta`, `data`).
    snapshots: sled::Tree,
}

impl StateMachineStore {
    pub fn open(
        db: &sled::Db,
        config: Arc<AppState>,
        intent: Arc<IntentState>,
    ) -> Result<Self, sled::Error> {
        Ok(StateMachineStore {
            config,
            intent,
            meta: db.open_tree("raft_sm_meta")?,
            snapshots: db.open_tree("raft_snapshot")?,
        })
    }

    // `StorageError` is `openraft`'s own type, sized by its own variants —
    // not something a caller in this crate can shrink.
    #[allow(clippy::result_large_err)]
    fn read_meta(&self) -> Result<SmMeta, StorageError<NodeId>> {
        Ok(self
            .meta
            .get(SM_META_KEY)
            .map_err(|e| StorageIOError::read(&e))?
            .map(|v| serde_json::from_slice(&v))
            .transpose()
            .map_err(|e| StorageIOError::read(&e))?
            .unwrap_or_default())
    }

    #[allow(clippy::result_large_err)] // same as `read_meta` above
    fn write_meta(&self, meta: &SmMeta) -> Result<(), StorageError<NodeId>> {
        let bytes = serde_json::to_vec(meta).map_err(|e| StorageIOError::write(&e))?;
        self.meta
            .insert(SM_META_KEY, bytes)
            .map_err(|e| StorageIOError::write(&e))?;
        self.meta.flush().map_err(|e| StorageIOError::write(&e))?;
        Ok(())
    }

    /// Copies both stores (revisions, stage, actor, `applied_index`) and
    /// the HA meta. Runs on the state-machine worker, so no apply
    /// interleaves.
    #[allow(clippy::result_large_err)] // same as `read_meta` above
    fn copy(&self) -> Result<SnapshotCopy, StorageError<NodeId>> {
        let config_store = &self.config.store;
        let mut config = Vec::new();
        for (revision, bytes) in config_store
            .all_revisions()
            .map_err(|e| StorageIOError::read_state_machine(&e))?
        {
            let key = revision.to_be_bytes();
            let stage = match self
                .config
                .stage
                .get(key)
                .map_err(|e| StorageIOError::read_state_machine(&e))?
            {
                Some(v) => serde_json::from_slice(&v)
                    .map_err(|e| StorageIOError::read_state_machine(&e))?,
                None => Stage::promoted(),
            };
            let actor = self
                .config
                .actors
                .get(key)
                .map_err(|e| StorageIOError::read_state_machine(&e))?
                .map(|v| String::from_utf8_lossy(&v).into_owned());
            config.push(RevisionSnap {
                revision,
                bytes,
                stage,
                actor,
            });
        }
        let intent_store = &self.intent.store;
        Ok(SnapshotCopy {
            content: SnapshotContent {
                config,
                intent: intent_store
                    .all_revisions()
                    .map_err(|e| StorageIOError::read_state_machine(&e))?,
                config_applied: config_store
                    .applied_index()
                    .map_err(|e| StorageIOError::read_state_machine(&e))?,
                intent_applied: intent_store
                    .applied_index()
                    .map_err(|e| StorageIOError::read_state_machine(&e))?,
            },
            meta: self.read_meta()?,
        })
    }

    /// Replaces the config store (with its stage and actor trees) and the
    /// intent store by `content`, one transaction per database.
    #[allow(clippy::result_large_err)] // same as `read_meta` above
    fn replace_stores(&self, content: &SnapshotContent) -> Result<(), StorageError<NodeId>> {
        let config = &self.config;
        let revisions: Vec<_> = content
            .config
            .iter()
            .map(|r| (r.revision, r.bytes.clone()))
            .collect();
        let mut siblings = Vec::new();
        for r in &content.config {
            let key = r.revision.to_be_bytes().to_vec();
            siblings.push(SiblingWrite {
                tree: &config.stage,
                key: key.clone(),
                value: Some(
                    serde_json::to_vec(&r.stage)
                        .map_err(|e| StorageIOError::write_state_machine(&e))?,
                ),
            });
            if let Some(actor) = &r.actor {
                siblings.push(SiblingWrite {
                    tree: &config.actors,
                    key,
                    value: Some(actor.as_bytes().to_vec()),
                });
            }
        }
        config
            .store
            .replace_all_with(
                &revisions,
                content.config_applied,
                &[&config.stage, &config.actors],
                siblings,
            )
            .map_err(|e| StorageIOError::write_state_machine(&e))?;
        self.intent
            .store
            .replace_all(&content.intent, content.intent_applied)
            .map_err(|e| StorageIOError::write_state_machine(&e))?;

        // Wake live subscribers so they re-read from the replaced store.
        if let Some(r) = content.config.last() {
            let _ = config.updates.send(r.revision);
        }
        if let Some((r, _)) = content.intent.last() {
            let _ = self.intent.updates.send(*r);
        }
        Ok(())
    }
}

/// Persists `meta` + `data` as the current snapshot, in one transaction,
/// unless the stored one already reaches as far (`last_log_id` >= the new
/// one's). `build_snapshot` runs in a task of its own, concurrently with
/// `install_snapshot`, so a build taken before an install can finish after
/// it; the stored snapshot must never go backwards behind the purge point.
#[allow(clippy::result_large_err)] // `StorageError` is openraft's, see `read_meta`
fn save_snapshot(
    tree: &sled::Tree,
    meta: &SnapshotMeta<NodeId, openraft::BasicNode>,
    data: &[u8],
) -> Result<(), StorageError<NodeId>> {
    let sig = || Some(meta.signature());
    let meta_bytes =
        serde_json::to_vec(meta).map_err(|e| StorageIOError::write_snapshot(sig(), &e))?;
    let outcome = tree.transaction(|t| {
        if let Some(stored) = t.get(SNAPSHOT_META_KEY)? {
            let stored: SnapshotMeta<NodeId, openraft::BasicNode> =
                serde_json::from_slice(&stored).map_err(ConflictableTransactionError::Abort)?;
            if stored.last_log_id >= meta.last_log_id {
                return Ok(false);
            }
        }
        t.insert(SNAPSHOT_META_KEY, meta_bytes.as_slice())?;
        t.insert(SNAPSHOT_DATA_KEY, data)?;
        Ok(true)
    });
    let written = outcome.map_err(|e| match e {
        TransactionError::Storage(e) => StorageIOError::write_snapshot(sig(), &e),
        TransactionError::Abort(e) => StorageIOError::write_snapshot(sig(), &e),
    })?;
    if written {
        tree.flush()
            .map_err(|e| StorageIOError::write_snapshot(sig(), &e))?;
    }
    Ok(())
}

impl RaftSnapshotBuilder<TypeConfig> for SnapshotBuilder {
    async fn build_snapshot(&mut self) -> Result<Snapshot<TypeConfig>, StorageError<NodeId>> {
        // Only serializes the copy `get_snapshot_builder` took: `openraft`
        // runs this in parallel with later applies, so the live stores may
        // already be ahead of `self.copy.meta`.
        let copy = self.copy.as_ref().map_err(Clone::clone)?;
        let data = serde_json::to_vec(&copy.content)
            .map_err(|e| StorageIOError::read_state_machine(&e))?;
        let meta = &copy.meta;
        let snapshot_id = match &meta.last_applied_log {
            Some(id) => format!("{}-{}", id.leader_id, id.index),
            None => "empty".to_string(),
        };
        let snapshot_meta = SnapshotMeta {
            last_log_id: meta.last_applied_log,
            last_membership: meta.last_membership.clone(),
            snapshot_id,
        };
        save_snapshot(&self.snapshots, &snapshot_meta, &data)?;

        Ok(Snapshot {
            meta: snapshot_meta,
            snapshot: Box::new(std::io::Cursor::new(data)),
        })
    }
}

impl RaftStateMachine<TypeConfig> for Arc<StateMachineStore> {
    type SnapshotBuilder = SnapshotBuilder;

    async fn applied_state(
        &mut self,
    ) -> Result<
        (
            Option<LogId<NodeId>>,
            StoredMembership<NodeId, openraft::BasicNode>,
        ),
        StorageError<NodeId>,
    > {
        let meta = self.read_meta()?;
        Ok((meta.last_applied_log, meta.last_membership))
    }

    async fn apply<I>(&mut self, entries: I) -> Result<Vec<WriteResponse>, StorageError<NodeId>>
    where
        I: IntoIterator<Item = Entry<TypeConfig>> + Send,
        I::IntoIter: Send,
    {
        let mut meta = self.read_meta()?;
        let mut out = Vec::new();

        for entry in entries {
            meta.last_applied_log = Some(entry.log_id);

            let index = entry.log_id.index;
            let response = match entry.payload {
                EntryPayload::Blank => WriteResponse { revision: None },
                EntryPayload::Normal(req) => {
                    // Each step is skipped when its database already
                    // absorbed `index` — openraft re-delivers entries
                    // after a crash that lost `last_applied_log`, and the
                    // `applied_index` lives in the same `sled` transaction
                    // as the revision so a replay writes nothing. A skipped
                    // step has no new revision to report; nobody awaits the
                    // response of a replayed entry.
                    let revision = match req {
                        WriteRequest::Config {
                            bytes,
                            stage,
                            actor,
                        } => self
                            .config
                            .apply_entry(index, bytes, stage, actor.as_deref())
                            .map_err(|e| StorageIOError::write_state_machine(&e))?,
                        WriteRequest::Intent(bytes) => self
                            .intent
                            .apply_entry(index, bytes)
                            .map_err(|e| StorageIOError::write_state_machine(&e))?,
                        WriteRequest::Promote(revision) => {
                            // A promote of a revision this replica doesn't
                            // have (shouldn't happen — the promoted
                            // revision was itself a committed, and
                            // therefore already-applied, entry) is a
                            // no-op that still advances the index; the
                            // direct (non-HA) `promote_revision` call is
                            // what gives an accurate `404` to the caller.
                            self.config
                                .promote_entry(index, revision)
                                .map_err(|e| StorageIOError::write_state_machine(&e))?;
                            Some(revision)
                        }
                    };
                    WriteResponse { revision }
                }
                EntryPayload::Membership(ref membership) => {
                    meta.last_membership =
                        StoredMembership::new(Some(entry.log_id), membership.clone());
                    WriteResponse { revision: None }
                }
            };
            out.push(response);
        }

        // Persisted before returning — the "state machine flushes to disk
        // before returning from apply()" option `RaftLogStorage::
        // save_committed`'s doc names, which means the log store's default
        // (never persisting `committed`) is correct as-is.
        self.write_meta(&meta)?;
        Ok(out)
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        SnapshotBuilder {
            copy: self.copy(),
            snapshots: self.snapshots.clone(),
        }
    }

    async fn begin_receiving_snapshot(
        &mut self,
    ) -> Result<Box<std::io::Cursor<Vec<u8>>>, StorageError<NodeId>> {
        Ok(Box::new(std::io::Cursor::new(Vec::new())))
    }

    async fn install_snapshot(
        &mut self,
        meta: &SnapshotMeta<NodeId, openraft::BasicNode>,
        snapshot: Box<std::io::Cursor<Vec<u8>>>,
    ) -> Result<(), StorageError<NodeId>> {
        let content: SnapshotContent = serde_json::from_slice(snapshot.get_ref())
            .map_err(|e| StorageIOError::read_snapshot(Some(meta.signature()), &e))?;

        // Stores first, HA meta second: a crash in between leaves stores
        // whose `applied_index` already covers the snapshot, so the entries
        // openraft re-delivers from the older `last_applied_log` are
        // skipped rather than applied twice.
        self.replace_stores(&content)?;
        self.write_meta(&SmMeta {
            last_applied_log: meta.last_log_id,
            last_membership: meta.last_membership.clone(),
        })?;
        save_snapshot(&self.snapshots, meta, snapshot.get_ref())?;
        Ok(())
    }

    async fn get_current_snapshot(
        &mut self,
    ) -> Result<Option<Snapshot<TypeConfig>>, StorageError<NodeId>> {
        // Both keys in one transaction, so a concurrent `save_snapshot`
        // can never pair one snapshot's meta with another's data.
        let stored = self
            .snapshots
            .transaction(|t| {
                Ok::<_, ConflictableTransactionError<std::convert::Infallible>>(
                    match (t.get(SNAPSHOT_META_KEY)?, t.get(SNAPSHOT_DATA_KEY)?) {
                        (Some(meta), Some(data)) => Some((meta, data)),
                        _ => None,
                    },
                )
            })
            .map_err(|e| match e {
                TransactionError::Storage(e) => StorageIOError::read_snapshot(None, &e),
                TransactionError::Abort(never) => match never {},
            })?;
        let Some((meta, data)) = stored else {
            return Ok(None);
        };
        let meta: SnapshotMeta<NodeId, openraft::BasicNode> =
            serde_json::from_slice(&meta).map_err(|e| StorageIOError::read_snapshot(None, &e))?;
        Ok(Some(Snapshot {
            meta,
            snapshot: Box::new(std::io::Cursor::new(data.to_vec())),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::role::{Role, RoleHandle};
    use crate::store::Store;
    use openraft::{CommittedLeaderId, EntryPayload};

    struct TestDirs {
        _config: tempfile::TempDir,
        _intent: tempfile::TempDir,
        ha: tempfile::TempDir,
    }

    fn test_sm() -> (Arc<StateMachineStore>, TestDirs) {
        let config_dir = tempfile::tempdir().unwrap();
        let intent_dir = tempfile::tempdir().unwrap();
        let config = Arc::new(AppState::new(
            Arc::new(Store::open(config_dir.path()).unwrap()),
            None,
            RoleHandle::new(Role::Standalone),
        ));
        let intent = Arc::new(IntentState::new(
            Arc::new(Store::open(intent_dir.path()).unwrap()),
            RoleHandle::new(Role::Standalone),
            None,
        ));
        let ha_dir = tempfile::tempdir().unwrap();
        let db = sled::open(ha_dir.path()).unwrap();
        (
            Arc::new(StateMachineStore::open(&db, config, intent).unwrap()),
            TestDirs {
                _config: config_dir,
                _intent: intent_dir,
                ha: ha_dir,
            },
        )
    }

    fn blank_entry(index: u64) -> Entry<TypeConfig> {
        Entry {
            log_id: LogId::new(CommittedLeaderId::new(1, 0), index),
            payload: EntryPayload::Blank,
        }
    }

    fn normal_entry(index: u64, req: WriteRequest) -> Entry<TypeConfig> {
        Entry {
            log_id: LogId::new(CommittedLeaderId::new(1, 0), index),
            payload: EntryPayload::Normal(req),
        }
    }

    #[tokio::test]
    async fn applying_a_config_write_lands_it_in_the_config_store() {
        let (mut sm, _dirs) = test_sm();
        let entries = vec![normal_entry(
            1,
            WriteRequest::Config {
                bytes: b"pools: []".to_vec(),
                stage: crate::api::Stage::promoted(),
                actor: None,
            },
        )];
        let responses = sm.apply(entries).await.unwrap();
        assert_eq!(responses[0].revision, Some(1));

        let (revision, bytes) = sm.config.store.current().unwrap().unwrap();
        assert_eq!(revision, 1);
        assert_eq!(bytes, b"pools: []");
    }

    #[tokio::test]
    async fn applying_an_intent_write_lands_it_in_the_intent_store_not_config() {
        let (mut sm, _dirs) = test_sm();
        let entries = vec![normal_entry(
            1,
            WriteRequest::Intent(br#"{"op":"backend_add"}"#.to_vec()),
        )];
        sm.apply(entries).await.unwrap();

        assert!(sm.config.store.current().unwrap().is_none());
        assert!(sm.intent.store.current().unwrap().is_some());
    }

    #[tokio::test]
    async fn a_blank_entry_updates_last_applied_but_writes_nothing() {
        let (mut sm, _dirs) = test_sm();
        let responses = sm.apply(vec![blank_entry(1)]).await.unwrap();
        assert_eq!(responses[0].revision, None);

        let (last_applied, _) = sm.applied_state().await.unwrap();
        assert_eq!(last_applied.unwrap().index, 1);
    }

    #[tokio::test]
    async fn applied_state_persists_across_a_reopen() {
        let (mut sm, dirs) = test_sm();
        sm.apply(vec![blank_entry(5)]).await.unwrap();
        drop(sm);

        // Re-`open` the exact same `sled` db path a process restart would —
        // the config/intent `Store`s are reopened fresh too, but that's
        // fine here: this test is only checking the HA meta tree
        // (`last_applied_log`), not the config/intent content.
        let config_dir = tempfile::tempdir().unwrap();
        let intent_dir = tempfile::tempdir().unwrap();
        let config = Arc::new(AppState::new(
            Arc::new(Store::open(config_dir.path()).unwrap()),
            None,
            RoleHandle::new(Role::Standalone),
        ));
        let intent = Arc::new(IntentState::new(
            Arc::new(Store::open(intent_dir.path()).unwrap()),
            RoleHandle::new(Role::Standalone),
            None,
        ));
        let db = crate::store::reopen_when_unlocked(|| sled::open(dirs.ha.path()));
        let mut reopened = Arc::new(StateMachineStore::open(&db, config, intent).unwrap());

        let (last_applied, _) = reopened.applied_state().await.unwrap();
        assert_eq!(last_applied.unwrap().index, 5);
    }

    #[tokio::test]
    async fn snapshot_round_trip_restores_both_stores() {
        let (mut sm, _dirs) = test_sm();
        sm.apply(vec![normal_entry(
            1,
            WriteRequest::Config {
                bytes: b"pools: []".to_vec(),
                stage: crate::api::Stage::promoted(),
                actor: None,
            },
        )])
        .await
        .unwrap();
        sm.apply(vec![normal_entry(
            2,
            WriteRequest::Intent(br#"{"op":"backend_add"}"#.to_vec()),
        )])
        .await
        .unwrap();

        let snapshot = sm
            .get_snapshot_builder()
            .await
            .build_snapshot()
            .await
            .unwrap();

        // Install onto a fresh, empty state machine.
        let (mut fresh, _dirs2) = test_sm();
        fresh
            .install_snapshot(&snapshot.meta, snapshot.snapshot)
            .await
            .unwrap();

        assert_eq!(
            fresh.config.store.current().unwrap().unwrap().1,
            b"pools: []"
        );
        assert!(fresh.intent.store.current().unwrap().is_some());
    }

    #[tokio::test]
    async fn replaying_an_applied_config_entry_writes_no_second_revision() {
        let (mut sm, _dirs) = test_sm();
        sm.apply(vec![normal_entry(
            1,
            WriteRequest::Config {
                bytes: b"pools: []".to_vec(),
                stage: crate::api::Stage::promoted(),
                actor: None,
            },
        )])
        .await
        .unwrap();

        // What openraft does when `write_meta` was lost to a crash: hands
        // the very same entry to `apply` again.
        let replay = sm
            .config
            .apply_entry(
                1,
                b"pools: []".to_vec(),
                crate::api::Stage::promoted(),
                None,
            )
            .unwrap();
        assert_eq!(replay, None);
        assert_eq!(sm.config.store.current_revision().unwrap(), Some(1));
    }

    #[tokio::test]
    async fn replaying_an_applied_intent_entry_writes_no_second_revision() {
        let (mut sm, _dirs) = test_sm();
        let op = br#"{"op":"backend_add"}"#.to_vec();
        sm.apply(vec![normal_entry(1, WriteRequest::Intent(op.clone()))])
            .await
            .unwrap();

        let replay = sm.intent.apply_entry(1, op).unwrap();
        assert_eq!(replay, None);
        assert_eq!(sm.intent.store.current_revision().unwrap(), Some(1));
    }

    #[tokio::test]
    async fn stage_and_actor_land_in_the_revision_transaction() {
        let (sm, _dirs) = test_sm();
        let revision = sm
            .config
            .apply_entry(
                1,
                b"pools: []".to_vec(),
                crate::api::Stage::promoted(),
                Some("alice"),
            )
            .unwrap();
        assert_eq!(revision, Some(1));
        assert_eq!(sm.config.stage_of(1), crate::api::Stage::promoted());
        assert_eq!(sm.config.actor_of(1).as_deref(), Some("alice"));
        assert_eq!(sm.config.store.applied_index().unwrap(), Some(1));
    }

    #[tokio::test]
    async fn replaying_a_promote_entry_is_skipped_and_the_index_advances() {
        let (mut sm, _dirs) = test_sm();
        sm.apply(vec![
            normal_entry(
                1,
                WriteRequest::Config {
                    bytes: b"pools: []".to_vec(),
                    stage: crate::api::Stage {
                        promoted: false,
                        canary_groups: vec!["g".into()],
                    },
                    actor: None,
                },
            ),
            normal_entry(2, WriteRequest::Promote(1)),
        ])
        .await
        .unwrap();
        assert!(sm.config.stage_of(1).promoted);
        assert_eq!(sm.config.store.applied_index().unwrap(), Some(2));

        // Put the stage back to "canary" and replay entry 2: it must be
        // skipped entirely, so the stage stays un-promoted.
        sm.config
            .stage
            .insert(
                1u64.to_be_bytes(),
                serde_json::to_vec(&crate::api::Stage {
                    promoted: false,
                    canary_groups: vec!["g".into()],
                })
                .unwrap(),
            )
            .unwrap();
        assert_eq!(sm.config.promote_entry(2, 1).unwrap(), None);
        assert!(!sm.config.stage_of(1).promoted);
    }

    fn config_entry(index: u64, bytes: &[u8]) -> Entry<TypeConfig> {
        normal_entry(
            index,
            WriteRequest::Config {
                bytes: bytes.to_vec(),
                stage: crate::api::Stage::promoted(),
                actor: None,
            },
        )
    }

    /// Reopens the state machine on `dirs`' HA db (config/intent stores
    /// fresh — only the HA trees matter to the callers).
    fn reopen_ha(dirs: &TestDirs) -> (Arc<StateMachineStore>, TestDirs) {
        let (fresh, fresh_dirs) = test_sm();
        let db = crate::store::reopen_when_unlocked(|| sled::open(dirs.ha.path()));
        let sm = StateMachineStore::open(&db, fresh.config.clone(), fresh.intent.clone()).unwrap();
        (Arc::new(sm), fresh_dirs)
    }

    #[tokio::test]
    async fn install_replaces_a_non_empty_store() {
        let (mut follower, _f) = test_sm();
        follower
            .apply(vec![
                config_entry(1, b"a"),
                config_entry(2, b"b"),
                config_entry(3, b"stale"),
            ])
            .await
            .unwrap();
        let (mut leader, _l) = test_sm();
        leader
            .apply(vec![config_entry(1, b"a"), config_entry(2, b"b")])
            .await
            .unwrap();

        let snapshot = leader
            .get_snapshot_builder()
            .await
            .build_snapshot()
            .await
            .unwrap();
        follower
            .install_snapshot(&snapshot.meta, snapshot.snapshot)
            .await
            .unwrap();

        let want = leader.config.store.all_revisions().unwrap();
        assert_eq!(want.len(), 2);
        assert_eq!(follower.config.store.all_revisions().unwrap(), want);
        assert_eq!(follower.config.store.current_revision().unwrap(), Some(2));
    }

    #[tokio::test]
    async fn install_keeps_revision_numbers_and_metadata() {
        let (mut leader, _l) = test_sm();
        let canary = crate::api::Stage {
            promoted: false,
            canary_groups: vec!["g".into()],
        };
        leader
            .apply(vec![normal_entry(
                1,
                WriteRequest::Config {
                    bytes: b"a".to_vec(),
                    stage: canary.clone(),
                    actor: Some("bob".into()),
                },
            )])
            .await
            .unwrap();

        let snapshot = leader
            .get_snapshot_builder()
            .await
            .build_snapshot()
            .await
            .unwrap();
        let (mut follower, _f) = test_sm();
        follower
            .install_snapshot(&snapshot.meta, snapshot.snapshot)
            .await
            .unwrap();

        assert_eq!(follower.config.store.get(1).unwrap().unwrap(), b"a");
        assert_eq!(follower.config.stage_of(1), canary);
        assert_eq!(follower.config.actor_of(1).as_deref(), Some("bob"));
        assert_eq!(follower.config.store.applied_index().unwrap(), Some(1));
        let (last_applied, _) = follower.applied_state().await.unwrap();
        assert_eq!(last_applied.unwrap().index, 1);
    }

    #[tokio::test]
    async fn current_snapshot_is_the_last_built_one() {
        let (mut sm, dirs) = test_sm();
        sm.apply(vec![config_entry(1, b"a")]).await.unwrap();
        let built = sm
            .get_snapshot_builder()
            .await
            .build_snapshot()
            .await
            .unwrap();

        let current = sm.get_current_snapshot().await.unwrap().unwrap();
        assert_eq!(current.meta.snapshot_id, built.meta.snapshot_id);
        drop(sm);

        let (mut reopened, _r) = reopen_ha(&dirs);
        let current = reopened.get_current_snapshot().await.unwrap().unwrap();
        assert_eq!(current.meta.snapshot_id, built.meta.snapshot_id);
        assert_eq!(current.meta.last_log_id, built.meta.last_log_id);
        assert_eq!(current.snapshot.get_ref(), built.snapshot.get_ref());
    }

    #[tokio::test]
    async fn a_late_build_never_replaces_a_newer_installed_snapshot() {
        // openraft runs `build_snapshot` in a spawned task, concurrently
        // with `install_snapshot` on the state-machine worker.
        let (mut sm, dirs) = test_sm();
        sm.apply((1..=3).map(|i| config_entry(i, format!("c{i}").as_bytes())))
            .await
            .unwrap();
        let mut late_builder = sm.get_snapshot_builder().await;

        let (mut leader, _l) = test_sm();
        leader
            .apply((1..=5).map(|i| config_entry(i, format!("c{i}").as_bytes())))
            .await
            .unwrap();
        let newer = leader
            .get_snapshot_builder()
            .await
            .build_snapshot()
            .await
            .unwrap();
        sm.install_snapshot(&newer.meta, newer.snapshot)
            .await
            .unwrap();

        let late = late_builder.build_snapshot().await.unwrap();
        assert_eq!(late.meta.last_log_id.unwrap().index, 3);

        let current = sm.get_current_snapshot().await.unwrap().unwrap();
        assert_eq!(current.meta.snapshot_id, newer.meta.snapshot_id);
        assert_eq!(current.meta.last_log_id.unwrap().index, 5);
        drop((sm, late_builder));

        let (mut reopened, _r) = reopen_ha(&dirs);
        let current = reopened.get_current_snapshot().await.unwrap().unwrap();
        assert_eq!(current.meta.last_log_id.unwrap().index, 5);
    }

    #[tokio::test]
    async fn a_builder_is_not_affected_by_later_applies() {
        let (mut leader, _l) = test_sm();
        leader
            .apply((1..=3).map(|i| config_entry(i, format!("c{i}").as_bytes())))
            .await
            .unwrap();
        let mut builder = leader.get_snapshot_builder().await;
        leader
            .apply((4..=6).map(|i| config_entry(i, format!("c{i}").as_bytes())))
            .await
            .unwrap();

        let snapshot = builder.build_snapshot().await.unwrap();
        assert_eq!(snapshot.meta.last_log_id.unwrap().index, 3);
        let content: SnapshotContent = serde_json::from_slice(snapshot.snapshot.get_ref()).unwrap();
        assert_eq!(content.config.len(), 3);
        assert_eq!(content.config_applied, Some(3));

        let (mut fresh, _f) = test_sm();
        fresh
            .install_snapshot(&snapshot.meta, snapshot.snapshot)
            .await
            .unwrap();
        fresh
            .apply((4..=6).map(|i| config_entry(i, format!("c{i}").as_bytes())))
            .await
            .unwrap();
        assert_eq!(
            fresh.config.store.all_revisions().unwrap(),
            leader.config.store.all_revisions().unwrap()
        );
    }

    #[tokio::test]
    async fn a_promote_of_a_missing_revision_still_advances_the_index() {
        let (mut sm, _dirs) = test_sm();
        let responses = sm
            .apply(vec![normal_entry(1, WriteRequest::Promote(99))])
            .await
            .unwrap();
        assert_eq!(responses[0].revision, Some(99));
        assert_eq!(sm.config.store.applied_index().unwrap(), Some(1));
        assert_eq!(sm.config.store.current_revision().unwrap(), None);
    }
}
