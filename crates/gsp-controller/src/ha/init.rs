//! Leader-side initialization of the replicated registries ("The cluster's
//! tunnel network" and "Choosing the source",
//! `docs/superpowers/specs/2026-10-03-ha-replicated-address-allocation-design.md`).
//!
//! Until the cluster records its tunnel network every registry entry is
//! rejected `NotInitialized`. [`initialize_registries`] runs on every node.
//! While this node is the leader and nothing is recorded yet it asks the
//! voters what pre-HA data they hold ([`choose_source`]) and proposes either
//! `SetTunnelNetwork` (nobody has data) or `Import` of the one source's
//! data. With several sources and no `--ha-import-source` it initializes
//! nothing and logs an `ERROR` instead of guessing. Once a value is recorded
//! each node checks its own `--tunnel-network` against it, logs one `ERROR`
//! naming both on a mismatch (writes then answer `503`, see `registry::ha`),
//! and the task ends.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use super::cluster_state::ClusterState;
use super::import::{ImportContent, PreHaSummary};
use super::peers::peer_url;
use super::routes::Whoami;
use super::{HaHandle, NodeId, WriteRequest};
use crate::addresses::Network;

/// Which node's pre-HA data the cluster imports (`--ha-import-source`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportPolicy {
    /// No flag: ask every voter; import the one node that has data.
    Auto,
    /// `--ha-import-source <id>`: import that node's data; ask nobody else.
    Source(NodeId),
    /// `--ha-import-source none`: import nothing.
    None,
}

/// What the leader does once it knows who holds pre-HA data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Nobody has data (or none is wanted): record the leader's network.
    SetNetwork,
    /// Import this node's data.
    Import(NodeId),
    /// Several nodes have data and no source was named: initialize nothing.
    Blocked(Vec<(NodeId, PreHaSummary)>),
}

/// Picks what to do from the voters' `whoami` summaries.
pub fn choose_source(summaries: &[(NodeId, PreHaSummary)], policy: ImportPolicy) -> Decision {
    match policy {
        ImportPolicy::None => Decision::SetNetwork,
        ImportPolicy::Source(id) => Decision::Import(id),
        ImportPolicy::Auto => {
            let with_data: Vec<_> = summaries
                .iter()
                .filter(|(_, summary)| !summary.is_empty())
                .copied()
                .collect();
            match with_data.as_slice() {
                [] => Decision::SetNetwork,
                [(id, _)] => Decision::Import(*id),
                _ => Decision::Blocked(with_data),
            }
        }
    }
}

const POLL: Duration = Duration::from_millis(500);

pub async fn initialize_registries(
    ha: Arc<HaHandle>,
    cluster: Arc<ClusterState>,
    local_network: Option<Network>,
    policy: ImportPolicy,
) {
    // The last thing logged while waiting, so a state that persists is
    // reported once, not at every poll.
    let mut last_note = String::new();
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
                    if let Err(note) = initialize_once(&ha, local_network, policy).await {
                        if note != last_note {
                            tracing::warn!("{note}");
                            last_note = note;
                        }
                    }
                }
            }
            Err(e) => tracing::warn!(error = %e.0, "reading the cluster's tunnel network"),
        }
        tokio::time::sleep(POLL).await;
    }
}

/// One initialization attempt by the leader; `Err` says why it must wait.
async fn initialize_once(
    ha: &HaHandle,
    local_network: Option<Network>,
    policy: ImportPolicy,
) -> Result<(), String> {
    let voters = voters(ha);
    let asked: Vec<(NodeId, String)> = match policy {
        ImportPolicy::None => Vec::new(),
        ImportPolicy::Source(id) => voters.iter().filter(|(v, _)| *v == id).cloned().collect(),
        ImportPolicy::Auto => voters,
    };
    if let ImportPolicy::Source(id) = policy {
        if asked.is_empty() {
            return Err(format!(
                "--ha-import-source {id} names a node that is not a voter; registry writes stay \
                 503 until the flag names a voter or `none`"
            ));
        }
    }
    let mut summaries = Vec::new();
    for (id, addr) in &asked {
        let summary = if *id == ha.node_id {
            ha.pre_ha.summary
        } else {
            whoami(ha, addr)
                .await
                .map_err(|e| format!("waiting for node {id} ({addr}) to answer /raft/whoami: {e}"))?
                .pre_ha
        };
        summaries.push((*id, summary));
    }
    let request = match choose_source(&summaries, policy) {
        Decision::SetNetwork => {
            WriteRequest::SetTunnelNetwork(local_network.map(|n| n.to_string()))
        }
        Decision::Import(id) => {
            let addr = asked
                .iter()
                .find(|(v, _)| *v == id)
                .map(|(_, addr)| addr.as_str())
                .expect("a source is always one of the nodes asked");
            let content = fetch_content(ha, id, addr).await?;
            WriteRequest::Import(Box::new(content))
        }
        Decision::Blocked(nodes) => {
            return Err(format!(
                "several nodes hold pre-HA registry data ({}); nothing is initialized. Restart \
                 the nodes with --ha-import-source <node-id> to pick the one to import, or \
                 `none`; registry writes stay 503 until then",
                nodes
                    .iter()
                    .map(|(id, s)| format!(
                        "node {id}: {} origins, {} proxies, {} addresses",
                        s.origins, s.proxies, s.addresses
                    ))
                    .collect::<Vec<_>>()
                    .join("; ")
            ))
        }
    };
    let imported = matches!(request, WriteRequest::Import(_));
    match ha.raft.client_write(request).await {
        Ok(_) if imported => tracing::info!("imported pre-HA registrations into the cluster"),
        Ok(_) => tracing::info!(
            network = %name(local_network),
            "recorded the cluster's tunnel network"
        ),
        // Leadership moved mid-proposal; the next leader retries.
        Err(e) => tracing::debug!(error = %e, "initializing the registries failed"),
    }
    Ok(())
}

/// The current voters and their addresses.
fn voters(ha: &HaHandle) -> Vec<(NodeId, String)> {
    let metrics = ha.raft.metrics().borrow().clone();
    let membership = metrics.membership_config.membership();
    let ids: BTreeSet<NodeId> = membership.voter_ids().collect();
    membership
        .nodes()
        .filter(|(id, _)| ids.contains(id))
        .map(|(id, node)| (*id, node.addr.clone()))
        .collect()
}

fn authorized(ha: &HaHandle, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    match &ha.ha_token {
        Some(token) => req.bearer_auth(token),
        None => req,
    }
}

pub(super) async fn whoami(ha: &HaHandle, addr: &str) -> Result<Whoami, String> {
    let resp = authorized(ha, ha.forward.get(peer_url(addr, "/raft/whoami")))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("answered {}", resp.status()));
    }
    resp.json().await.map_err(|e| e.to_string())
}

/// The source's set-aside data: read locally when it is this node, fetched
/// from `/raft/pre-ha` otherwise.
async fn fetch_content(ha: &HaHandle, id: NodeId, addr: &str) -> Result<ImportContent, String> {
    let missing = || format!("--ha-import-source {id} names a node with no pre-HA data");
    if id == ha.node_id {
        let local = ha.pre_ha.clone();
        let node_id = ha.node_id;
        return tokio::task::spawn_blocking(move || {
            super::import::read_pre_ha(&local.data_dir, local.network, node_id)
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("reading this node's pre-HA data: {e}"))?
        .ok_or_else(missing);
    }
    let resp = authorized(ha, ha.forward.get(peer_url(addr, "/raft/pre-ha")))
        .send()
        .await
        .map_err(|e| format!("fetching node {id}'s pre-HA data: {e}"))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(missing());
    }
    if !resp.status().is_success() {
        return Err(format!(
            "node {id} answered {} to /raft/pre-ha",
            resp.status()
        ));
    }
    resp.json().await.map_err(|e| e.to_string())
}

fn name(network: Option<Network>) -> String {
    network.map_or_else(|| "none".into(), |n| n.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn some(n: usize) -> PreHaSummary {
        PreHaSummary {
            origins: n,
            proxies: 0,
            addresses: n,
        }
    }

    #[test]
    fn several_sources_without_a_policy_initialize_nothing() {
        let both = [(1, some(2)), (2, some(1)), (3, PreHaSummary::default())];
        assert_eq!(
            choose_source(&both, ImportPolicy::Auto),
            Decision::Blocked(vec![(1, some(2)), (2, some(1))])
        );
    }

    #[test]
    fn one_source_is_imported_and_none_sets_the_network() {
        let one = [(1, PreHaSummary::default()), (2, some(3))];
        assert_eq!(choose_source(&one, ImportPolicy::Auto), Decision::Import(2));
        let none = [(1, PreHaSummary::default()), (2, PreHaSummary::default())];
        assert_eq!(
            choose_source(&none, ImportPolicy::Auto),
            Decision::SetNetwork
        );
    }

    #[test]
    fn a_named_policy_decides_whatever_the_summaries_say() {
        let both = [(1, some(2)), (2, some(1))];
        assert_eq!(
            choose_source(&both, ImportPolicy::Source(2)),
            Decision::Import(2)
        );
        assert_eq!(
            choose_source(&both, ImportPolicy::None),
            Decision::SetNetwork
        );
    }
}
