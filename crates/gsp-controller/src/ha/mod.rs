//! Intra-tier controller HA (phase 12 slice 6, `docs/10` "Intra-tier HA
//! (design)", ADR 21 in `docs/09`) — embedded `openraft`, **one Raft group
//! per controller tier replicating both the config and intent logs
//! together**. `sled` (`crate::store::Store`) is unchanged as the state
//! machine; `openraft` only adds a replicated log and leader election on
//! top of it.
//!
//! **Scope cut for this slice, stated up front**: HA and the `slave` role
//! are not combined yet. A tier either runs `--role standalone` with
//! optional `--ha-peers` (this module), or `--role slave` with no HA
//! (slices 1/4's `parent_client`/`intent::relay`, untouched) — never both at
//! once. Combining them needs the upward relay to run leader-only with its
//! cursor promoted to replicated state, exactly as designed in `docs/10`;
//! that wiring is real, designed, and deliberately not built in this slice
//! to keep it reviewable. `main.rs` enforces the cut at startup.
//!
//! Module layout, mirroring `openraft`'s own `raft-kv-memstore` example
//! (adapted: `sled`-backed instead of in-memory, `axum` instead of
//! `actix-web`, and the state machine applies straight into the existing
//! [`crate::api::AppState`] / [`crate::intent::api::IntentState`] rather
//! than a bespoke KV):
//!
//! - [`log_store`] — `RaftLogStorage` + `RaftLogReader`, backed by two
//!   `sled` trees (`log`, `meta`) so the Raft log itself survives a
//!   restart, not just the state machine it replicates.
//! - [`state_machine`] — `RaftStateMachine` + `RaftSnapshotBuilder`, wraps
//!   the config and intent [`crate::api::AppState`]/
//!   [`crate::intent::api::IntentState`] directly and calls their existing
//!   `apply_revision` (the same bypass a `slave`'s relay already uses) —
//!   applying a committed Raft entry is not a new code path, just a new
//!   *source* for the exact write `submit()` already made when HA is off.
//! - [`network`] — `RaftNetworkFactory` + `RaftNetwork` over `reqwest`,
//!   posting to peers' `/raft/*` routes.
//! - [`routes`] — the `axum` handlers for `/raft/append`, `/raft/vote`,
//!   `/raft/snapshot`, gated by a peer-only `--ha-token`.
//! - [`client`] — [`client::propose_write`], the one function `api::submit`
//!   and `intent::api::submit_intent` call instead of `apply_revision`
//!   directly when HA is on: proposes a Raft write, and transparently
//!   forwards to the current leader over HTTP if this replica isn't it —
//!   no client of this API (`gsp`, `gsp-ui`, `curl`) ever needs to know HA
//!   exists.

pub mod client;
pub mod log_store;
pub mod network;
pub mod routes;
pub mod state_machine;

use std::sync::Arc;

use serde::{Deserialize, Serialize};

pub type NodeId = u64;

/// One proposed write, tagged by which log it targets — the Raft log
/// carries both the config and intent logs' entries interleaved, since one
/// tier is one Raft group (`docs/10`: "not two independent groups").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WriteRequest {
    /// Carries the submission's [`crate::api::Stage`] alongside the bytes —
    /// phase 12 slice 7 — so every replica's state machine applies the
    /// exact same rollout visibility a direct (non-HA) `submit()` would
    /// have, not just the same bytes.
    Config {
        bytes: Vec<u8>,
        stage: crate::api::Stage,
        /// Phase 12 slice 8's `X-Actor`, if any — carried through the Raft
        /// log so every replica's state machine attributes the revision to
        /// the same actor, not just whichever node happened to originate
        /// the `client_write` call.
        actor: Option<String>,
    },
    Intent(Vec<u8>),
    /// Phase 12 slice 7: flips an existing config revision's `Stage::promoted`
    /// to `true` in place — the same operation `AppState::promote_revision`
    /// does directly when HA is off.
    Promote(u64),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteResponse {
    /// `None` only for a Raft-internal entry (blank leader no-op,
    /// membership change) that never came from `client_write` — a real
    /// [`WriteRequest`] always produces `Some`.
    pub revision: Option<u64>,
}

openraft::declare_raft_types!(
    pub TypeConfig:
        D = WriteRequest,
        R = WriteResponse,
        NodeId = NodeId,
        Node = openraft::BasicNode,
        Entry = openraft::Entry<TypeConfig>,
        SnapshotData = std::io::Cursor<Vec<u8>>,
        AsyncRuntime = openraft::TokioRuntime,
);

pub type Raft = openraft::Raft<TypeConfig>;

/// Shorthand aliases for `openraft`'s error types instantiated against this
/// crate's [`NodeId`]/`BasicNode` — mirrors the `typ` module every
/// `openraft` example defines, so call sites don't repeat the full
/// parameter list at every use.
pub mod typ {
    use openraft::BasicNode;

    use super::{NodeId, TypeConfig};

    pub type RaftError<E = openraft::error::Infallible> = openraft::error::RaftError<NodeId, E>;
    pub type RPCError<E = openraft::error::Infallible> =
        openraft::error::RPCError<NodeId, BasicNode, RaftError<E>>;
    pub type ClientWriteError = openraft::error::ClientWriteError<NodeId, BasicNode>;
    pub type ForwardToLeader = openraft::error::ForwardToLeader<NodeId, BasicNode>;
    pub type ClientWriteResponse = openraft::raft::ClientWriteResponse<TypeConfig>;
}

/// Everything a controller process needs to participate in its tier's Raft
/// group — shared (via `Arc`) between [`crate::api::AppState`] and
/// [`crate::intent::api::IntentState`], since both logs are replicated by
/// the same group.
pub struct HaHandle {
    pub raft: Raft,
    pub node_id: NodeId,
    /// This node's own advertised address (`host:port`, no scheme) — needed
    /// to tell whether a `ForwardToLeader` response actually names *this*
    /// node (can happen transiently right after an election).
    pub self_addr: String,
    /// Peer-only shared secret gating `/raft/*` — a separate secret from
    /// `--auth-token` (client-facing) and `--parent-token`
    /// (slave-to-parent), matching every other cross-service credential
    /// pair in this fleet.
    pub ha_token: Option<Arc<str>>,
}
