//! Leader-side initialization of the replicated registries ("The cluster's
//! tunnel network",
//! `docs/superpowers/specs/2026-10-03-ha-replicated-address-allocation-design.md`).
//!
//! Until the cluster records its tunnel network every registry entry is
//! rejected `NotInitialized`. [`initialize_registries`] runs on every node
//! and, while this node is the leader and nothing is recorded yet, proposes
//! `SetTunnelNetwork` with the leader's own `--tunnel-network`. Once a value
//! is recorded each node checks its own flag against it, logs one `ERROR`
//! naming both on a mismatch (writes then answer `503`, see
//! `registry::ha`), and the task ends.

use std::sync::Arc;
use std::time::Duration;

use super::cluster_state::ClusterState;
use super::{HaHandle, WriteRequest};
use crate::addresses::Network;

/// How the first initialization chooses the recorded network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportPolicy {
    /// Record the leader's `--tunnel-network` as given.
    Never,
}

const POLL: Duration = Duration::from_millis(200);

pub async fn initialize_registries(
    ha: Arc<HaHandle>,
    cluster: Arc<ClusterState>,
    local_network: Option<Network>,
    import: ImportPolicy,
) {
    let ImportPolicy::Never = import;
    loop {
        match cluster.network() {
            Ok(Some(recorded)) => {
                if recorded != local_network {
                    tracing::error!(
                        local = %name(local_network),
                        recorded = %name(recorded),
                        "this node's --tunnel-network differs from the cluster's recorded \
                         network; registry writes are refused here until the flag is fixed"
                    );
                }
                return;
            }
            Ok(None) => {
                let is_leader = ha.raft.metrics().borrow().current_leader == Some(ha.node_id);
                if is_leader {
                    let request =
                        WriteRequest::SetTunnelNetwork(local_network.map(|n| n.to_string()));
                    match ha.raft.client_write(request).await {
                        Ok(_) => tracing::info!(
                            network = %name(local_network),
                            "recorded the cluster's tunnel network"
                        ),
                        // Leadership moved mid-proposal; the next leader retries.
                        Err(e) => {
                            tracing::debug!(error = %e, "recording the tunnel network failed")
                        }
                    }
                }
            }
            Err(e) => tracing::warn!(error = %e.0, "reading the cluster's tunnel network"),
        }
        tokio::time::sleep(POLL).await;
    }
}

fn name(network: Option<Network>) -> String {
    network.map_or_else(|| "none".into(), |n| n.to_string())
}
