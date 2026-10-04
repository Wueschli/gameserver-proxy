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
//! Registry entries (`RegisterOrigin`/`RegisterProxy`, `Release`, `Touch`)
//! apply through [`super::apply_registry`] into the two registries and the
//! shared address book, against the network recorded in
//! [`ClusterState`] by `SetTunnelNetwork` — never the node's own flag.
//!
//! Snapshots are a main path, not a corner case: `openraft` purges the log
//! per [`super::raft_config`]'s snapshot policy, and a follower or learner
//! that lags behind the purge point is caught up by snapshot. A snapshot
//! carries every config, intent and registry revision *at its number* (with
//! the config revision's stage and actor, and each registry's `current`
//! map), the address book, the cluster state and each store's
//! `applied_index`; installing one **replaces** all of them. `get_snapshot_builder` copies
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

use super::apply_registry;
use super::cluster_state::{ClusterSnapshot, ClusterState};
use super::import;
use super::{NodeId, TypeConfig, WriteRequest, WriteResponse};
use crate::addresses::{AddressBook, BookSnapshot, Network, Rejection, Role};
use crate::api::{AppState, Stage};
use crate::intent::api::IntentState;
use crate::peers::api::PeersState;
use crate::proxy_peers::api::ProxyPeersState;
use crate::registry::RegistrySnapshot;
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

/// The full snapshot content: every revision of every log at its number,
/// the registries' `current` maps, the address book, the cluster state and
/// each store's `applied_index` — enough to replace every database the
/// state machine drives exactly. Every section is required: a snapshot
/// persisted before the registries were replicated (before 2026-10-03)
/// lacks some and is rejected on install, never read as empty
/// ([`decode_snapshot`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotContent {
    pub config: Vec<RevisionSnap>,
    pub intent: Vec<(u64, Vec<u8>)>,
    pub config_applied: Option<u64>,
    pub intent_applied: Option<u64>,
    pub peers: RegistrySnapshot,
    pub proxy_peers: RegistrySnapshot,
    pub book: BookSnapshot,
    pub cluster: ClusterSnapshot,
}

/// Parses a received snapshot. One in an older format (a section missing)
/// is an error naming that, so a follower never installs it as empty
/// registries and an empty address book.
fn decode_snapshot(bytes: &[u8]) -> Result<SnapshotContent, std::io::Error> {
    serde_json::from_slice(bytes).map_err(|e| {
        std::io::Error::other(format!(
            "the snapshot format is not this build's (a snapshot written before \
             2026-10-03 is not supported; start this node from empty storage): {e}"
        ))
    })
}

/// The registries and the shared address book the state machine applies
/// registry entries into.
pub struct Registries {
    pub peers: Arc<PeersState>,
    pub proxy_peers: Arc<ProxyPeersState>,
    pub book: Arc<AddressBook>,
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
    peers: Arc<PeersState>,
    proxy_peers: Arc<ProxyPeersState>,
    book: Arc<AddressBook>,
    /// The replicated network registry entries are applied against.
    cluster: Arc<ClusterState>,
    /// `Some((node id, data dir))` to warn about unimported pre-HA data.
    pre_ha_notice: Option<(NodeId, std::path::PathBuf)>,
    meta: sled::Tree,
    /// The last built or installed snapshot (`meta`, `data`).
    snapshots: sled::Tree,
}

impl StateMachineStore {
    pub fn open(
        db: &sled::Db,
        config: Arc<AppState>,
        intent: Arc<IntentState>,
        registries: Registries,
    ) -> Result<Self, sled::Error> {
        Ok(StateMachineStore {
            config,
            intent,
            peers: registries.peers,
            proxy_peers: registries.proxy_peers,
            book: registries.book,
            cluster: Arc::new(ClusterState::open(db)?),
            pre_ha_notice: None,
            meta: db.open_tree("raft_sm_meta")?,
            snapshots: db.open_tree("raft_snapshot")?,
        })
    }

    /// Makes an applied `Import` warn on a node that set its own pre-HA data
    /// aside but was not the source: that data stays untouched.
    pub fn with_pre_ha_notice(mut self, node_id: NodeId, data_dir: std::path::PathBuf) -> Self {
        self.pre_ha_notice = Some((node_id, data_dir));
        self
    }

    fn warn_if_unimported(&self, content: &import::ImportContent) {
        let Some((node_id, data_dir)) = &self.pre_ha_notice else {
            return;
        };
        if *node_id == content.source {
            return;
        }
        for name in ["peers", "proxy-peers", "tunnel-addresses"] {
            let dir = data_dir.join(format!("{name}.pre-ha"));
            if dir.exists() {
                tracing::warn!(
                    dir = %dir.display(),
                    source = content.source,
                    "the cluster imported node {}'s pre-HA registrations; this node's own \
                     pre-HA data is left untouched and was not imported",
                    content.source
                );
            }
        }
    }

    /// The replicated cluster state (the recorded tunnel network).
    pub fn cluster(&self) -> &Arc<ClusterState> {
        &self.cluster
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

    /// Copies every store (revisions, stage, actor, registries' `current`,
    /// the address book, the cluster state, each `applied_index`) and the
    /// HA meta. Runs on the state-machine worker, so no apply interleaves.
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
                peers: self
                    .peers
                    .snapshot()
                    .map_err(|e| StorageIOError::read_state_machine(&e))?,
                proxy_peers: self
                    .proxy_peers
                    .snapshot()
                    .map_err(|e| StorageIOError::read_state_machine(&e))?,
                book: self
                    .book
                    .snapshot()
                    .map_err(|e| StorageIOError::read_state_machine(&e))?,
                cluster: self
                    .cluster
                    .snapshot()
                    .map_err(|e| StorageIOError::read_state_machine(&e))?,
            },
            meta: self.read_meta()?,
        })
    }

    /// `SetTunnelNetwork` at `index`: records the cluster's network once.
    /// An unparsable network is a deterministic rejection (every replica
    /// parses the same string), never a storage error.
    #[allow(clippy::result_large_err)] // same as `read_meta` above
    #[allow(clippy::needless_pass_by_value)] // owned like the other state-machine writes
    fn set_tunnel_network(
        &self,
        network: Option<String>,
        index: u64,
    ) -> Result<WriteResponse, StorageError<NodeId>> {
        let parsed = match network.as_deref().map(Network::parse).transpose() {
            Ok(parsed) => parsed,
            Err(e) => return Ok(WriteResponse::Rejected(Rejection::InvalidNetwork(e))),
        };
        self.cluster
            .record(parsed, index)
            .map_err(|e| StorageIOError::write_state_machine(&e))?;
        Ok(WriteResponse::Recorded)
    }

    /// Replaces the config store (with its stage and actor trees), the
    /// intent store, both registries, the address book and the cluster
    /// state by `content`, one transaction per database.
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
        self.peers
            .replace(&content.peers)
            .map_err(|e| StorageIOError::write_state_machine(&e))?;
        self.proxy_peers
            .replace(&content.proxy_peers)
            .map_err(|e| StorageIOError::write_state_machine(&e))?;
        self.book
            .replace(&content.book)
            .map_err(|e| StorageIOError::write_state_machine(&e))?;
        self.cluster
            .replace(&content.cluster)
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
                EntryPayload::Blank => WriteResponse::Revision(None),
                EntryPayload::Normal(req) => {
                    // Each step is skipped when its database already
                    // absorbed `index` — openraft re-delivers entries
                    // after a crash that lost `last_applied_log`, and the
                    // `applied_index` lives in the same `sled` transaction
                    // as the revision so a replay writes nothing. A skipped
                    // step has no new revision to report; nobody awaits the
                    // response of a replayed entry.
                    match req {
                        WriteRequest::Config {
                            bytes,
                            stage,
                            actor,
                        } => WriteResponse::Revision(
                            self.config
                                .apply_entry(index, bytes, stage, actor.as_deref())
                                .map_err(|e| StorageIOError::write_state_machine(&e))?,
                        ),
                        WriteRequest::Intent(bytes) => WriteResponse::Revision(
                            self.intent
                                .apply_entry(index, bytes)
                                .map_err(|e| StorageIOError::write_state_machine(&e))?,
                        ),
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
                            WriteResponse::Revision(Some(revision))
                        }
                        // Registry entries read nothing node-local: `now`
                        // comes from the entry, the network from the
                        // replicated `ClusterState`.
                        WriteRequest::RegisterOrigin { reg, now } => apply_registry::register(
                            &self.peers,
                            &self.book,
                            &self.cluster,
                            reg,
                            now,
                            index,
                        )?,
                        WriteRequest::RegisterProxy { reg, now } => apply_registry::register(
                            &self.proxy_peers,
                            &self.book,
                            &self.cluster,
                            reg,
                            now,
                            index,
                        )?,
                        WriteRequest::Release {
                            role: Role::Origin,
                            name,
                        } => apply_registry::release(
                            &self.peers,
                            &self.book,
                            &self.cluster,
                            &name,
                            index,
                        )?,
                        WriteRequest::Release {
                            role: Role::Proxy,
                            name,
                        } => apply_registry::release(
                            &self.proxy_peers,
                            &self.book,
                            &self.cluster,
                            &name,
                            index,
                        )?,
                        WriteRequest::Touch {
                            role: Role::Origin,
                            name,
                            now,
                        } => apply_registry::touch(
                            &self.peers,
                            &self.book,
                            &self.cluster,
                            &name,
                            now,
                            index,
                        )?,
                        WriteRequest::Touch {
                            role: Role::Proxy,
                            name,
                            now,
                        } => apply_registry::touch(
                            &self.proxy_peers,
                            &self.book,
                            &self.cluster,
                            &name,
                            now,
                            index,
                        )?,
                        WriteRequest::SetTunnelNetwork(network) => {
                            self.set_tunnel_network(network, index)?
                        }
                        WriteRequest::Import(content) => {
                            let response = import::apply_import(
                                &self.peers,
                                &self.proxy_peers,
                                &self.book,
                                &self.cluster,
                                &content,
                                index,
                            )?;
                            self.warn_if_unimported(&content);
                            response
                        }
                    }
                }
                EntryPayload::Membership(ref membership) => {
                    meta.last_membership =
                        StoredMembership::new(Some(entry.log_id), membership.clone());
                    WriteResponse::Revision(None)
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
        let content = decode_snapshot(snapshot.get_ref())
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
    use crate::addresses::{
        api::claim_error_response, expand_backends, AddressBook, ClaimError, Network, Outcome,
        Rejection, Role as AddrRole,
    };
    use crate::peers::PeerRegistration;
    use crate::proxy_peers::ProxyRegistration;
    use crate::role::{Role, RoleHandle};
    use crate::store::Store;
    use axum::http::StatusCode;
    use openraft::{CommittedLeaderId, EntryPayload};
    use std::net::IpAddr;

    struct TestDirs {
        _stores: tempfile::TempDir,
        ha: tempfile::TempDir,
    }

    /// Every store the state machine drives, freshly opened under one
    /// temporary directory. The address book is opened **without** a
    /// network: HA claims must take theirs from the replicated cluster
    /// state, never from the book.
    fn open_sm(db: &sled::Db) -> (StateMachineStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = |sub: &str| Arc::new(Store::open(&dir.path().join(sub)).unwrap());
        let config = Arc::new(AppState::new(
            store("config"),
            None,
            RoleHandle::new(Role::Standalone),
        ));
        let intent = Arc::new(IntentState::new(
            store("intent"),
            RoleHandle::new(Role::Standalone),
            None,
        ));
        let book = Arc::new(AddressBook::open(&dir.path().join("addresses"), None).unwrap());
        let peers = Arc::new(PeersState::new(store("peers"), None, book.clone()));
        let proxy_peers = Arc::new(ProxyPeersState::new(
            store("proxy-peers"),
            None,
            book.clone(),
        ));
        let sm = StateMachineStore::open(
            db,
            config,
            intent,
            Registries {
                peers,
                proxy_peers,
                book,
            },
        )
        .unwrap();
        (sm, dir)
    }

    fn test_sm() -> (Arc<StateMachineStore>, TestDirs) {
        let ha_dir = tempfile::tempdir().unwrap();
        let db = sled::open(ha_dir.path()).unwrap();
        let (sm, stores) = open_sm(&db);
        (
            Arc::new(sm),
            TestDirs {
                _stores: stores,
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
        assert_eq!(responses[0], WriteResponse::Revision(Some(1)));

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
        assert_eq!(responses[0], WriteResponse::Revision(None));

        let (last_applied, _) = sm.applied_state().await.unwrap();
        assert_eq!(last_applied.unwrap().index, 1);
    }

    #[tokio::test]
    async fn applied_state_persists_across_a_reopen() {
        let (mut sm, dirs) = test_sm();
        sm.apply(vec![blank_entry(5)]).await.unwrap();
        drop(sm);

        // Re-`open` the exact same `sled` db path a process restart would —
        // the other `Store`s are reopened fresh too, but that's fine here:
        // this test is only checking the HA meta tree (`last_applied_log`),
        // not the stores' content.
        let (mut reopened, _r) = reopen_ha(&dirs);

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
    fn reopen_ha(dirs: &TestDirs) -> (Arc<StateMachineStore>, tempfile::TempDir) {
        let db = crate::store::reopen_when_unlocked(|| sled::open(dirs.ha.path()));
        let (sm, stores) = open_sm(&db);
        (Arc::new(sm), stores)
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
    async fn install_rejects_a_snapshot_from_before_the_registries_were_replicated() {
        let (mut leader, _l) = test_sm();
        leader.apply(vec![config_entry(1, b"a")]).await.unwrap();
        let snapshot = leader
            .get_snapshot_builder()
            .await
            .build_snapshot()
            .await
            .unwrap();
        // The format before 2026-10-03 had only the config and intent
        // sections.
        let mut old: serde_json::Value =
            serde_json::from_slice(snapshot.snapshot.get_ref()).unwrap();
        for section in ["peers", "proxy_peers", "book", "cluster"] {
            old.as_object_mut().unwrap().remove(section).unwrap();
        }

        let (mut follower, _f) = test_sm();
        follower
            .apply(vec![config_entry(1, b"mine")])
            .await
            .unwrap();
        let err = follower
            .install_snapshot(
                &snapshot.meta,
                Box::new(std::io::Cursor::new(serde_json::to_vec(&old).unwrap())),
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("snapshot format"),
            "the error names the cause: {err}"
        );
        // Nothing was replaced.
        assert_eq!(follower.config.store.get(1).unwrap().unwrap(), b"mine");
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
        assert_eq!(responses[0], WriteResponse::Revision(Some(99)));
        assert_eq!(sm.config.store.applied_index().unwrap(), Some(1));
        assert_eq!(sm.config.store.current_revision().unwrap(), None);
    }

    // ---- Registry entries (Task 5) ----

    const NOW: u64 = 1_000;
    const PUBKEY: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

    fn origin(name: &str, address: Option<&str>, backends: &[&str]) -> PeerRegistration {
        PeerRegistration {
            name: name.into(),
            pubkey: PUBKEY.into(),
            endpoint: None,
            backends: backends
                .iter()
                .map(std::string::ToString::to_string)
                .collect(),
            tunnel_address: address.map(Into::into),
        }
    }

    fn proxy(name: &str, address: Option<&str>) -> ProxyRegistration {
        ProxyRegistration {
            name: name.into(),
            pubkey: PUBKEY.into(),
            endpoint: "203.0.113.1:51820".into(),
            tunnel_address: address.map(Into::into),
            boot_id: None,
        }
    }

    fn register_origin(reg: PeerRegistration, now: u64) -> WriteRequest {
        WriteRequest::RegisterOrigin { reg, now }
    }

    async fn apply_one(
        sm: &mut Arc<StateMachineStore>,
        index: u64,
        req: WriteRequest,
    ) -> WriteResponse {
        sm.apply(vec![normal_entry(index, req)])
            .await
            .unwrap()
            .remove(0)
    }

    /// A state machine whose cluster recorded `network` at index 1.
    async fn sm_with_network(network: &str) -> (Arc<StateMachineStore>, TestDirs) {
        let (mut sm, dirs) = test_sm();
        let response = apply_one(
            &mut sm,
            1,
            WriteRequest::SetTunnelNetwork(Some(network.into())),
        )
        .await;
        assert_eq!(response, WriteResponse::Recorded);
        (sm, dirs)
    }

    /// The mixed entries 2..=21 of `two_state_machines_fed_the_same_entries_agree`.
    fn mixed_entries() -> Vec<Entry<TypeConfig>> {
        (2u64..=21)
            .map(|i| {
                let now = NOW + i;
                let req = match i % 4 {
                    0 => register_origin(origin(&format!("o{}", i % 5), None, &[":25565"]), now),
                    1 => WriteRequest::RegisterProxy {
                        reg: proxy(&format!("p{}", i % 3), None),
                        now,
                    },
                    2 => WriteRequest::Release {
                        role: AddrRole::Origin,
                        name: format!("o{}", (i + 1) % 5),
                    },
                    _ => WriteRequest::Touch {
                        role: AddrRole::Proxy,
                        name: format!("p{}", i % 3),
                        now,
                    },
                };
                normal_entry(i, req)
            })
            .collect()
    }

    #[tokio::test]
    async fn two_state_machines_fed_the_same_entries_agree() {
        let (mut a, _a) = sm_with_network("10.60.0.0/24").await;
        let (mut b, _b) = sm_with_network("10.60.0.0/24").await;
        let ra = a.apply(mixed_entries()).await.unwrap();
        let rb = b.apply(mixed_entries()).await.unwrap();
        assert_eq!(ra, rb);
        assert!(ra
            .iter()
            .any(|r| matches!(r, WriteResponse::Registered { .. })));

        assert_eq!(
            a.peers.all_current().unwrap(),
            b.peers.all_current().unwrap()
        );
        assert_eq!(
            a.proxy_peers.all_current().unwrap(),
            b.proxy_peers.all_current().unwrap()
        );
        assert!(!a.proxy_peers.all_current().unwrap().is_empty());
        assert_eq!(a.book.entries().unwrap(), b.book.entries().unwrap());
        assert!(!a.book.entries().unwrap().is_empty());
        assert_eq!(
            a.peers.store.all_revisions().unwrap(),
            b.peers.store.all_revisions().unwrap()
        );
        assert_eq!(
            a.proxy_peers.store.all_revisions().unwrap(),
            b.proxy_peers.store.all_revisions().unwrap()
        );
    }

    #[tokio::test]
    async fn interleaved_registrations_never_share_an_address() {
        let (mut sm, _d) = sm_with_network("10.60.0.0/24").await;
        let entries: Vec<_> = (0..64u64)
            .map(|i| {
                normal_entry(
                    i + 2,
                    register_origin(origin(&format!("o{i}"), None, &[]), NOW),
                )
            })
            .collect();
        let responses = sm.apply(entries).await.unwrap();
        let addresses: std::collections::HashSet<IpAddr> = responses
            .iter()
            .map(|r| match r {
                WriteResponse::Registered { address, .. } => *address,
                other => panic!("expected a registration, got {other:?}"),
            })
            .collect();
        assert_eq!(addresses.len(), 64);
    }

    #[tokio::test]
    async fn rejections_match_the_non_ha_handler() {
        let net = Network::parse("10.60.0.0/24").unwrap();
        let (mut sm, _d) = sm_with_network("10.60.0.0/24").await;
        // The same claims through the non-HA path, on a book of its own.
        let dir = tempfile::tempdir().unwrap();
        let direct = AddressBook::open(dir.path(), Some(net)).unwrap();
        let direct_claim = |reg: &PeerRegistration| -> Result<(), Rejection> {
            let a = direct
                .claim(AddrRole::Origin, &reg.name, reg.requested_address(), NOW)
                .map_err(|e| match e {
                    ClaimError::Rejected(r) => r,
                    ClaimError::Storage(e) => panic!("storage: {e}"),
                })?;
            expand_backends(&reg.backends, a.address).map_err(Rejection::BackendHost)?;
            Ok(())
        };

        let holder = origin("a", Some("10.60.0.5"), &[]);
        direct_claim(&holder).unwrap();
        assert!(matches!(
            apply_one(&mut sm, 2, register_origin(holder, NOW)).await,
            WriteResponse::Registered { .. }
        ));

        let cases = [
            (origin("b", Some("10.60.0.5"), &[]), StatusCode::CONFLICT),
            (origin("a", Some("10.60.0.6"), &[]), StatusCode::CONFLICT),
            (
                origin("c", Some("10.61.0.1"), &[]),
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (
                origin("d", None, &["10.60.0.99:25565"]),
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
        ];
        for (i, (reg, status)) in cases.into_iter().enumerate() {
            let want = direct_claim(&reg).unwrap_err();
            let got = apply_one(&mut sm, 3 + i as u64, register_origin(reg, NOW)).await;
            assert_eq!(got, WriteResponse::Rejected(want.clone()));
            assert_eq!(
                claim_error_response(&ClaimError::Rejected(want)).status(),
                status
            );
        }
        // The backend-host rejection keeps the claim, as the handler does.
        assert!(sm.book.get(AddrRole::Origin, "d").unwrap().is_some());
        assert_eq!(sm.peers.current_for("d").unwrap(), None);
        assert_eq!(sm.peers.store.applied_index().unwrap(), Some(6));
        assert_eq!(sm.book.applied_index().unwrap(), Some(6));
    }

    #[tokio::test]
    async fn a_crash_between_book_and_registry_completes_from_last_outcome() {
        let net = Network::parse("10.60.0.0/24").unwrap();
        let (mut sm, _d) = sm_with_network("10.60.0.0/24").await;
        // The book step of entry 9 ran; the crash lost the registry step.
        let granted = match sm
            .book
            .claim_at(AddrRole::Origin, "a", None, NOW, Some(net), 9)
            .unwrap()
        {
            Outcome::Granted(a) => a,
            other => panic!("expected a grant, got {other:?}"),
        };
        let book_before = sm.book.entries().unwrap();

        let response = apply_one(
            &mut sm,
            9,
            register_origin(origin("a", None, &[":25565"]), NOW),
        )
        .await;
        assert_eq!(
            response,
            WriteResponse::Registered {
                revision: 1,
                address: granted.address
            }
        );
        let stored = sm.peers.current_for("a").unwrap().unwrap();
        assert_eq!(stored.requested_address(), Some(granted.address));
        assert_eq!(stored.backends, vec![format!("{}:25565", granted.address)]);
        assert_eq!(sm.book.entries().unwrap(), book_before);
        assert_eq!(sm.peers.store.applied_index().unwrap(), Some(9));
    }

    #[tokio::test]
    async fn a_storage_failure_is_a_storage_error_not_a_rejection() {
        let (mut sm, _d) = sm_with_network("10.60.0.0/24").await;
        // A corrupt `applied_index` makes every book step fail to read it.
        sm.book
            .meta_tree()
            .insert(b"applied_index", b"bad".to_vec())
            .unwrap();
        let result = sm
            .apply(vec![normal_entry(
                2,
                register_origin(origin("a", None, &[]), NOW),
            )])
            .await;
        assert!(result.is_err(), "expected a StorageError, got {result:?}");
        assert_eq!(sm.peers.store.applied_index().unwrap(), None);
        assert_eq!(sm.peers.current_for("a").unwrap(), None);
    }

    #[tokio::test]
    async fn a_rejected_register_then_a_touch_replays_cleanly() {
        let (mut sm, _d) = sm_with_network("10.60.0.0/24").await;
        apply_one(
            &mut sm,
            2,
            register_origin(origin("holder", Some("10.60.0.5"), &[]), NOW),
        )
        .await;
        let entries = || {
            vec![
                normal_entry(
                    10,
                    register_origin(origin("other", Some("10.60.0.5"), &[]), NOW),
                ),
                normal_entry(
                    11,
                    WriteRequest::Touch {
                        role: AddrRole::Origin,
                        name: "holder".into(),
                        now: NOW + 50,
                    },
                ),
            ]
        };
        let responses = sm.apply(entries()).await.unwrap();
        assert!(matches!(
            responses[0],
            WriteResponse::Rejected(Rejection::Held { .. })
        ));
        assert_eq!(responses[1], WriteResponse::Touched);

        let revisions = sm.peers.store.all_revisions().unwrap();
        let book = sm.book.entries().unwrap();
        let outcome = sm.book.last_outcome().unwrap();
        sm.apply(entries()).await.unwrap();
        assert_eq!(sm.peers.store.all_revisions().unwrap(), revisions);
        assert_eq!(sm.book.entries().unwrap(), book);
        assert_eq!(sm.book.last_outcome().unwrap(), outcome);
        assert_eq!(sm.peers.store.applied_index().unwrap(), Some(11));
        assert_eq!(sm.book.applied_index().unwrap(), Some(11));
    }

    #[tokio::test]
    async fn a_registry_entry_before_initialization_is_rejected_not_fatal() {
        let (mut sm, _d) = test_sm();
        let response = apply_one(&mut sm, 1, register_origin(origin("a", None, &[]), NOW)).await;
        assert_eq!(response, WriteResponse::Rejected(Rejection::NotInitialized));
        assert_eq!(sm.peers.store.applied_index().unwrap(), Some(1));
        assert_eq!(sm.book.applied_index().unwrap(), Some(1));
        assert!(sm.book.entries().unwrap().is_empty());
        assert_eq!(sm.peers.store.current_revision().unwrap(), None);
    }

    #[tokio::test]
    async fn snapshot_round_trip_carries_registries_and_the_book() {
        let (mut leader, _l) = sm_with_network("10.60.0.0/24").await;
        let mut moved = origin("a", None, &[]);
        moved.endpoint = Some("198.51.100.1:51820".into());
        leader
            .apply(vec![
                normal_entry(2, register_origin(origin("a", None, &[]), NOW)),
                normal_entry(3, register_origin(moved, NOW + 1)),
                normal_entry(
                    4,
                    WriteRequest::RegisterProxy {
                        reg: proxy("p", None),
                        now: NOW,
                    },
                ),
                normal_entry(
                    5,
                    WriteRequest::Release {
                        role: AddrRole::Origin,
                        name: "a".into(),
                    },
                ),
                normal_entry(6, register_origin(origin("b", None, &[]), NOW + 2)),
            ])
            .await
            .unwrap();
        assert_eq!(leader.peers.store.current_revision().unwrap(), Some(4));

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

        for (f, l) in [
            (&follower.peers.store, &leader.peers.store),
            (&follower.proxy_peers.store, &leader.proxy_peers.store),
        ] {
            assert_eq!(f.all_revisions().unwrap(), l.all_revisions().unwrap());
            assert_eq!(f.applied_index().unwrap(), l.applied_index().unwrap());
        }
        // Origin entries advance the peers index only; the proxy registry
        // last absorbed entry 4.
        assert_eq!(follower.peers.store.applied_index().unwrap(), Some(6));
        assert_eq!(follower.proxy_peers.store.applied_index().unwrap(), Some(4));
        assert_eq!(
            follower.peers.current_entry("b").unwrap(),
            leader.peers.current_entry("b").unwrap()
        );
        assert_eq!(follower.peers.current_for("a").unwrap(), None);
        assert_eq!(
            follower.proxy_peers.all_current().unwrap(),
            leader.proxy_peers.all_current().unwrap()
        );
        assert_eq!(
            follower.book.entries().unwrap(),
            leader.book.entries().unwrap()
        );
        assert_eq!(follower.book.applied_index().unwrap(), Some(6));
        assert_eq!(
            follower.book.last_outcome().unwrap(),
            leader.book.last_outcome().unwrap()
        );
        assert_eq!(
            follower.cluster().network().unwrap(),
            Some(Some(Network::parse("10.60.0.0/24").unwrap()))
        );

        // Both continue identically: the next revision is 5 on each.
        let next = || register_origin(origin("c", None, &[]), NOW + 3);
        let on_leader = apply_one(&mut leader, 7, next()).await;
        let on_follower = apply_one(&mut follower, 7, next()).await;
        assert_eq!(on_leader, on_follower);
        assert!(matches!(
            on_follower,
            WriteResponse::Registered { revision: 5, .. }
        ));
    }
}
