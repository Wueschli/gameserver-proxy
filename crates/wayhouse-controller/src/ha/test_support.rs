//! A single-node Raft group on temporary storage, for tests of the HA routes.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use super::cluster_state::ClusterState;
use super::state_machine::{Registries, StateMachineStore};
use super::{client, log_store, network, raft_config, HaHandle, Raft, SNAPSHOT_AFTER};
use crate::addresses::AddressBook;
use crate::api::AppState;
use crate::intent::api::IntentState;
use crate::peers::api::PeersState;
use crate::proxy_peers::api::ProxyPeersState;
use crate::role::{Role, RoleHandle};
use crate::store::Store;

/// A node with id `node_id` that has elected itself leader of a one-voter
/// cluster whose membership lists `addr`.
pub(crate) async fn single_node(
    node_id: u64,
    addr: &str,
) -> (Arc<HaHandle>, Arc<ClusterState>, tempfile::TempDir) {
    let (handle, cluster, _plugins, dir) = single_node_with_plugins(node_id, addr).await;
    (handle, cluster, dir)
}

/// [`single_node`], also handing out the state machine's plugin store.
pub(crate) async fn single_node_with_plugins(
    node_id: u64,
    addr: &str,
) -> (
    Arc<HaHandle>,
    Arc<ClusterState>,
    crate::plugins::PluginStore,
    tempfile::TempDir,
) {
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
    let peers = PeersState::new(store("peers"), None, book.clone());
    let proxy_peers = ProxyPeersState::new(store("proxy-peers"), None, book.clone());
    let db = sled::open(dir.path().join("ha")).unwrap();
    let sm = Arc::new(
        StateMachineStore::open(
            &db,
            config,
            intent,
            Registries {
                peers: Arc::new(peers),
                proxy_peers: Arc::new(proxy_peers),
                book,
            },
        )
        .unwrap(),
    );
    let cluster = sm.cluster().clone();
    let plugins = sm.plugins().clone();
    let raft: Raft = openraft::Raft::new(
        node_id,
        Arc::new(raft_config(SNAPSHOT_AFTER).validate().unwrap()),
        network::Network::new(None),
        log_store::LogStore::open(&db).unwrap(),
        sm,
    )
    .await
    .unwrap();
    raft.initialize(BTreeMap::from([(node_id, openraft::BasicNode::new(addr))]))
        .await
        .unwrap();
    raft.wait(Some(Duration::from_secs(10)))
        .current_leader(node_id, "single node elects itself")
        .await
        .unwrap();
    let handle = Arc::new(HaHandle {
        raft,
        node_id,
        ha_token: None,
        forward: client::forward_client(client::FORWARD_TIMEOUT),
        pre_ha: crate::ha::import::LocalPreHa::default(),
        secret_keys: crate::plugins::secrets::KeyringHandle::default(),
    });
    (handle, cluster, plugins, dir)
}
