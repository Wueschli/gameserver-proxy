//! `RaftStateMachine` + `RaftSnapshotBuilder`: applying a committed
//! [`super::WriteRequest`] means calling the same
//! [`crate::api::AppState::apply_revision`] /
//! [`crate::intent::api::IntentState::apply_revision`] a direct (non-HA)
//! write already used — see `crate::ha`'s module doc. `sled` (via those two
//! `Store`s) is the actual durable content; this module only adds the
//! bookkeeping `openraft` needs on top (last-applied log id, membership,
//! and a snapshot for a follower whose log entries have been purged — which
//! never happens in this slice, see the note on `build_snapshot` below).

use std::sync::Arc;

use openraft::storage::{RaftStateMachine, Snapshot};
use openraft::{
    Entry, EntryPayload, LogId, RaftSnapshotBuilder, SnapshotMeta, StorageError, StorageIOError,
    StoredMembership,
};
use serde::{Deserialize, Serialize};

use super::{NodeId, TypeConfig, WriteRequest, WriteResponse};
use crate::api::AppState;
use crate::intent::api::IntentState;

const SM_META_KEY: &[u8] = b"sm_meta";

#[derive(Debug, Default, Serialize, Deserialize)]
struct SmMeta {
    last_applied_log: Option<LogId<NodeId>>,
    last_membership: StoredMembership<NodeId, openraft::BasicNode>,
}

/// The full snapshot content: every revision from both logs, enough to
/// rebuild a from-scratch replica's config+intent `Store`s exactly.
#[derive(Serialize, Deserialize)]
struct SnapshotContent {
    config_revisions: Vec<(u64, Vec<u8>)>,
    intent_revisions: Vec<(u64, Vec<u8>)>,
}

pub struct StateMachineStore {
    config: Arc<AppState>,
    intent: Arc<IntentState>,
    meta: sled::Tree,
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
}

impl RaftSnapshotBuilder<TypeConfig> for Arc<StateMachineStore> {
    async fn build_snapshot(&mut self) -> Result<Snapshot<TypeConfig>, StorageError<NodeId>> {
        // Exercised only if a follower ever needs log entries this node has
        // purged — which never happens in this slice (the log store never
        // purges; see `docs/10` "Intra-tier HA (design)"'s accepted
        // "log grows unbounded, compact later" tradeoff). Implemented
        // correctly anyway rather than stubbed, since `openraft` requires
        // the trait regardless and a half-correct snapshot would be a
        // silent landmine for whenever compaction *is* added.
        let config_revisions = self
            .config
            .store
            .revisions_after(0)
            .map_err(|e| StorageIOError::read_state_machine(&e))?;
        let intent_revisions = self
            .intent
            .store
            .revisions_after(0)
            .map_err(|e| StorageIOError::read_state_machine(&e))?;
        let content = SnapshotContent {
            config_revisions,
            intent_revisions,
        };
        let data =
            serde_json::to_vec(&content).map_err(|e| StorageIOError::read_state_machine(&e))?;

        let meta = self.read_meta()?;
        let snapshot_id = match &meta.last_applied_log {
            Some(id) => format!("{}-{}", id.leader_id, id.index),
            None => "empty".to_string(),
        };
        let snapshot_meta = SnapshotMeta {
            last_log_id: meta.last_applied_log,
            last_membership: meta.last_membership,
            snapshot_id,
        };

        Ok(Snapshot {
            meta: snapshot_meta,
            snapshot: Box::new(std::io::Cursor::new(data)),
        })
    }
}

impl RaftStateMachine<TypeConfig> for Arc<StateMachineStore> {
    type SnapshotBuilder = Self;

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

            let response = match entry.payload {
                EntryPayload::Blank => WriteResponse { revision: None },
                EntryPayload::Normal(req) => {
                    let revision = match req {
                        WriteRequest::Config {
                            bytes,
                            stage,
                            actor,
                        } => self
                            .config
                            .apply_revision_with_stage_and_actor(bytes, stage, actor.as_deref())
                            .map_err(|e| StorageIOError::write_state_machine(&e))?,
                        WriteRequest::Intent(bytes) => self
                            .intent
                            .apply_revision(bytes)
                            .map_err(|e| StorageIOError::write_state_machine(&e))?,
                        WriteRequest::Promote(revision) => {
                            // Best-effort: a promote of a revision this
                            // replica doesn't have (shouldn't happen — the
                            // revision that's being promoted was itself a
                            // committed, and therefore already-applied,
                            // entry) is silently a no-op rather than
                            // failing the whole `apply` batch; the direct
                            // (non-HA) `promote_revision` call is what gives
                            // an accurate `404` to the caller.
                            let _ = self
                                .config
                                .promote_revision(revision)
                                .map_err(|e| StorageIOError::write_state_machine(&e))?;
                            revision
                        }
                    };
                    WriteResponse {
                        revision: Some(revision),
                    }
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
        self.clone()
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

        // Replay every revision straight through `apply_revision` — the
        // installing node's config/intent `Store`s start empty (a snapshot
        // is only ever installed on a follower catching up from nothing),
        // so re-numbering from 1 upward reproduces the source exactly.
        for (_, bytes) in content.config_revisions {
            self.config
                .apply_revision(bytes)
                .map_err(|e| StorageIOError::write_state_machine(&e))?;
        }
        for (_, bytes) in content.intent_revisions {
            self.intent
                .apply_revision(bytes)
                .map_err(|e| StorageIOError::write_state_machine(&e))?;
        }

        self.write_meta(&SmMeta {
            last_applied_log: meta.last_log_id,
            last_membership: meta.last_membership.clone(),
        })?;
        Ok(())
    }

    async fn get_current_snapshot(
        &mut self,
    ) -> Result<Option<Snapshot<TypeConfig>>, StorageError<NodeId>> {
        // No separate "current snapshot" cache kept: `build_snapshot` is
        // cheap (a linear read of both `Store`s) and this path is only ever
        // hit alongside `build_snapshot` in the unpurged-log world this
        // slice ships, so there is nothing to gain from also persisting a
        // copy of the last-built snapshot.
        Ok(None)
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
        let db = sled::open(dirs.ha.path()).unwrap();
        let mut reopened = Arc::new(StateMachineStore::open(&db, config, intent).unwrap());

        let (last_applied, _) = reopened.applied_state().await.unwrap();
        assert_eq!(last_applied.unwrap().index, 5);
    }

    #[tokio::test]
    async fn snapshot_round_trip_replays_every_revision() {
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

        let snapshot = sm.build_snapshot().await.unwrap();

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
}
