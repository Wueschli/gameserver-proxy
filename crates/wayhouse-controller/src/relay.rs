//! What the two upward relays ([`crate::parent_client`] for config,
//! [`crate::intent::relay`] for intent) share: how a relayed revision is
//! landed when the tier is a Raft group, and which node runs the relay.
//!
//! Without HA a `slave` tier is one process that writes its store directly.
//! With HA only the **leader** runs the relay (`docs/10` "Intra-tier HA"):
//! every follower subscribing independently would apply the same parent
//! revision on its own and number it differently, corrupting the replicated
//! log. The leader proposes each parent revision as a `RelayConfig` /
//! `RelayIntent` entry carrying the parent revision number, which the state
//! machine stores as the relay cursor in the same transaction as the
//! revision. A newly elected leader resumes from that replicated cursor, and
//! an entry whose parent revision is not above the cursor (a deposed
//! leader's proposal that committed after its successor's) is skipped on
//! every replica alike.

use std::time::Duration;

use crate::ha::{typ, HaHandle, WriteRequest};
use crate::store::StoreError;

/// How often a node that is not the leader checks whether it became one.
pub const LEADER_POLL: Duration = Duration::from_millis(500);

#[derive(Debug, thiserror::Error)]
pub enum RelayError {
    /// This node is not (or no longer) the Raft leader; the relay stops and
    /// waits until it is again.
    #[error("this node is not the raft leader")]
    NotLeader,
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("raft write failed: {0}")]
    Raft(String),
}

/// Proposes one relay entry and waits for it to commit and apply.
pub async fn propose(ha: &HaHandle, req: WriteRequest) -> Result<(), RelayError> {
    match ha.raft.client_write(req).await {
        Ok(_) => Ok(()),
        Err(openraft::error::RaftError::APIError(typ::ClientWriteError::ForwardToLeader(_))) => {
            Err(RelayError::NotLeader)
        }
        Err(e) => Err(RelayError::Raft(e.to_string())),
    }
}

/// Returns once `is_leader` is true, checking every [`LEADER_POLL`].
pub async fn wait_until_leader(is_leader: impl Fn() -> bool) {
    while !is_leader() {
        tokio::time::sleep(LEADER_POLL).await;
    }
}
