//! The cluster's replicated tunnel network ("The cluster's tunnel network",
//! `docs/superpowers/specs/2026-10-03-ha-replicated-address-allocation-design.md`).
//!
//! Registry entries are applied against **one** network every replica
//! agrees on, never against a node's own `--tunnel-network`. The first
//! `SetTunnelNetwork` entry (or, later, an `Import`) records it together with
//! the `initialized` marker; it is recorded once and never changed. Until
//! then a registry entry is rejected with `Rejection::NotInitialized`.
//!
//! Lives in a tree of the HA `sled` database (`cluster_state`), next to the
//! state machine's own meta tree.

use serde::{Deserialize, Serialize};
use sled::transaction::{ConflictableTransactionError, TransactionError};

use crate::addresses::{Network, StorageFailure};

/// The log index the network was recorded at (big-endian `u64`); its
/// presence is the `initialized` marker.
const INITIALIZED_KEY: &[u8] = b"initialized_at";
/// The recorded network, JSON `Option<Network>` (`null` = pin-only).
const NETWORK_KEY: &[u8] = b"network";

/// The cluster state as a Raft snapshot carries it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterSnapshot {
    /// The log index the network was recorded at; `None` = not initialized.
    pub initialized_at: Option<u64>,
    pub network: Option<Network>,
}

pub struct ClusterState {
    tree: sled::Tree,
}

fn failure<E: std::fmt::Debug>(e: E) -> StorageFailure {
    StorageFailure(format!("cluster state: {e:?}"))
}

fn decode_index(bytes: &[u8]) -> Result<u64, StorageFailure> {
    let raw: [u8; 8] = bytes
        .try_into()
        .map_err(|_| failure(format!("initialized_at is {} bytes, not 8", bytes.len())))?;
    Ok(u64::from_be_bytes(raw))
}

impl ClusterState {
    pub fn open(db: &sled::Db) -> Result<Self, sled::Error> {
        Ok(ClusterState {
            tree: db.open_tree("cluster_state")?,
        })
    }

    /// The recorded network: outer `None` = the cluster is not initialized
    /// yet, `Some(None)` = initialized in pin-only mode.
    pub fn network(&self) -> Result<Option<Option<Network>>, StorageFailure> {
        let snap = self.snapshot()?;
        Ok(snap.initialized_at.map(|_| snap.network))
    }

    /// Records `network` as the cluster's network at log `index`, together
    /// with the `initialized` marker, in one transaction. Recorded once: when
    /// the cluster is already initialized (by this index on a replay, or by
    /// an earlier entry) nothing is written and `false` is returned.
    pub fn record(&self, network: Option<Network>, index: u64) -> Result<bool, StorageFailure> {
        let value = serde_json::to_vec(&network).map_err(failure)?;
        let written = self
            .tree
            .transaction(|t| {
                if t.get(INITIALIZED_KEY)?.is_some() {
                    return Ok(false);
                }
                t.insert(INITIALIZED_KEY, &index.to_be_bytes()[..])?;
                t.insert(NETWORK_KEY, value.as_slice())?;
                Ok::<_, ConflictableTransactionError<std::convert::Infallible>>(true)
            })
            .map_err(|e| match e {
                TransactionError::Storage(e) => failure(e),
                TransactionError::Abort(never) => match never {},
            })?;
        if written {
            self.tree.flush().map_err(failure)?;
        }
        Ok(written)
    }

    /// An owned copy for a Raft snapshot.
    pub fn snapshot(&self) -> Result<ClusterSnapshot, StorageFailure> {
        let initialized_at = self
            .tree
            .get(INITIALIZED_KEY)
            .map_err(failure)?
            .map(|b| decode_index(&b))
            .transpose()?;
        let network = match self.tree.get(NETWORK_KEY).map_err(failure)? {
            Some(b) => serde_json::from_slice(&b).map_err(failure)?,
            None => None,
        };
        Ok(ClusterSnapshot {
            initialized_at,
            network,
        })
    }

    /// Replaces the cluster state by `snapshot` in one transaction (a Raft
    /// snapshot install).
    pub fn replace(&self, snapshot: &ClusterSnapshot) -> Result<(), StorageFailure> {
        let value = serde_json::to_vec(&snapshot.network).map_err(failure)?;
        self.tree
            .transaction(|t| {
                match snapshot.initialized_at {
                    Some(index) => {
                        t.insert(INITIALIZED_KEY, &index.to_be_bytes()[..])?;
                        t.insert(NETWORK_KEY, value.as_slice())?;
                    }
                    None => {
                        t.remove(INITIALIZED_KEY)?;
                        t.remove(NETWORK_KEY)?;
                    }
                }
                Ok::<_, ConflictableTransactionError<std::convert::Infallible>>(())
            })
            .map_err(|e| match e {
                TransactionError::Storage(e) => failure(e),
                TransactionError::Abort(never) => match never {},
            })?;
        self.tree.flush().map_err(failure)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> (ClusterState, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = sled::open(dir.path()).unwrap();
        (ClusterState::open(&db).unwrap(), dir)
    }

    #[test]
    fn a_fresh_cluster_is_not_initialized() {
        let (c, _d) = state();
        assert_eq!(c.network().unwrap(), None);
    }

    #[test]
    fn the_network_is_recorded_once() {
        let (c, _d) = state();
        let net = Network::parse("10.60.0.0/24").unwrap();
        assert!(c.record(Some(net), 3).unwrap());
        assert_eq!(c.network().unwrap(), Some(Some(net)));
        // A replay of the same index, and any later record, change nothing.
        assert!(!c.record(Some(net), 3).unwrap());
        assert!(!c.record(None, 4).unwrap());
        assert_eq!(c.network().unwrap(), Some(Some(net)));
        assert_eq!(c.snapshot().unwrap().initialized_at, Some(3));
    }

    #[test]
    fn pin_only_is_initialized_without_a_network() {
        let (c, _d) = state();
        assert!(c.record(None, 1).unwrap());
        assert_eq!(c.network().unwrap(), Some(None));
    }

    #[test]
    fn replace_installs_and_clears() {
        let (c, _d) = state();
        c.record(None, 1).unwrap();
        let net = Network::parse("fd49::/64").unwrap();
        let snap = ClusterSnapshot {
            initialized_at: Some(7),
            network: Some(net),
        };
        c.replace(&snap).unwrap();
        assert_eq!(c.snapshot().unwrap(), snap);
        assert_eq!(c.network().unwrap(), Some(Some(net)));
        c.replace(&ClusterSnapshot::default()).unwrap();
        assert_eq!(c.network().unwrap(), None);
    }
}
