//! Intra-tier controller HA (phase 12 slice 6, `docs/10` "Intra-tier HA
//! (design)", ADR 21 in `docs/09`) — embedded `openraft`, **one Raft group
//! per controller tier replicating both the config and intent logs
//! together**. `sled` (`crate::store::Store`) is unchanged as the state
//! machine; `openraft` only adds a replicated log and leader election on
//! top of it.
//!
//! **HA and the `slave` role combine**: only the leader runs the upward
//! relay (`parent_client` for config, `intent::relay` for intent) and
//! proposes each parent revision as a [`WriteRequest::RelayConfig`] /
//! [`WriteRequest::RelayIntent`] entry. The relay cursor (the highest parent
//! revision absorbed) lives beside each log and is replicated with it, so a
//! newly elected leader resumes where the group left off. See [`crate::relay`].
//!
//! Module layout, mirroring `openraft`'s own `raft-kv-memstore` example
//! (adapted: `sled`-backed instead of in-memory, `axum` instead of
//! `actix-web`, and the state machine applies straight into the existing
//! [`crate::api::AppState`] / [`crate::intent::api::IntentState`] rather
//! than a bespoke KV):
//!
//! - [`log_store`] — `RaftLogStorage` + `RaftLogReader`, backed by two
//!   `sled` trees (`log`, `meta`) so the Raft log itself survives a
//!   restart, not just the state machine it replicates; `openraft` purges
//!   it up to each snapshot (keeping the last 1000 entries).
//! - [`state_machine`] — `RaftStateMachine` + `RaftSnapshotBuilder`, wraps
//!   the config and intent [`crate::api::AppState`]/
//!   [`crate::intent::api::IntentState`] directly and calls their
//!   index-aware `apply_entry` — applying a committed Raft entry is not a
//!   new code path, just a new *source* for the exact write `submit()`
//!   already made when HA is off. The log is purged per [`raft_config`]'s
//!   snapshot policy, so snapshots (which replace both stores on install)
//!   are how a lagging follower or a new learner catches up.
//! - [`apply_registry`] — how the state machine applies the registry
//!   entries (register, release, touch) into the two registries and the
//!   shared address book, deterministically and crash-idempotently.
//! - [`cluster_state`] — the replicated tunnel network registry entries are
//!   applied against, recorded once by `SetTunnelNetwork`.
//! - [`network`] — `RaftNetworkFactory` + `RaftNetwork` over `reqwest`,
//!   posting to peers' `/raft/*` routes.
//! - [`routes`] — the `axum` handlers for `/raft/append`, `/raft/vote`,
//!   `/raft/snapshot`, gated by a peer-only `--ha-token`.
//! - [`client`] — [`client::propose_write`], the one function `api::submit`
//!   and `intent::api::submit_intent` call instead of `apply_revision`
//!   directly when HA is on: proposes a Raft write, and transparently
//!   forwards to the current leader over HTTP if this replica isn't it —
//!   no client of this API (`wayhouse`, `wayhouse-ui`, `curl`) ever needs to know HA
//!   exists.

pub mod apply_registry;
pub mod client;
pub mod cluster_state;
pub mod import;
pub mod init;
pub mod log_store;
pub mod members;
pub mod network;
pub mod peers;
pub mod routes;
pub mod state_machine;
#[cfg(test)]
pub(crate) mod test_support;

use std::net::IpAddr;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::addresses::{Rejection, Role};
use crate::peers::PeerRegistration;
use crate::proxy_peers::ProxyRegistration;

pub type NodeId = u64;

/// Startup checks on the HA flags together: `--ha-peers` (bootstrap a cluster
/// with a static member list) and `--ha-join` (wait to be added) are mutually
/// exclusive, and either needs `--ha-node-id`.
pub fn check_flags(ha_peers: bool, ha_join: bool, node_id: Option<NodeId>) -> Result<(), String> {
    if ha_peers && ha_join {
        return Err(
            "--ha-join and --ha-peers are mutually exclusive: --ha-peers bootstraps a \
             new cluster, --ha-join starts a node that is added to a running one"
                .into(),
        );
    }
    let flag = if ha_join { "--ha-join" } else { "--ha-peers" };
    if (ha_peers || ha_join) && node_id.is_none() {
        return Err(format!("{flag} requires --ha-node-id"));
    }
    Ok(())
}

/// `/raft/*` and the membership API let a caller rewrite replicated state, so
/// HA never starts without a peer token (security review N1), and a token it
/// is given must not be short (O7). Unlike the client-facing `--auth-token`
/// there is no opt-out.
pub fn check_ha_token(ha_enabled: bool, token: Option<&str>) -> Result<(), String> {
    match token {
        None if ha_enabled => Err(
            "--ha-peers/--ha-join require --ha-token: without it the /raft/* and \
             /admin/ha/members endpoints accept any caller"
                .into(),
        ),
        t => wayhouse_http::policy::check_optional_secret("--ha-token", t),
    }
}

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
    /// A `slave` tier's upward relay of one config revision (promoted, as
    /// every relayed revision is), proposed by the leader only. Carries the
    /// parent's revision number, which becomes the relay cursor; applied at
    /// most once per parent revision, see [`crate::relay`].
    RelayConfig {
        bytes: Vec<u8>,
        parent_revision: u64,
    },
    /// [`WriteRequest::RelayConfig`] for the intent log.
    RelayIntent {
        bytes: Vec<u8>,
        parent_revision: u64,
    },
    /// Phase 12 slice 7: flips an existing config revision's `Stage::promoted`
    /// to `true` in place — the same operation `AppState::promote_revision`
    /// does directly when HA is off.
    Promote(u64),
    /// An origin's registration as received (address optional), with the
    /// proposing leader's clock — so `first_seen`/`last_seen` are identical
    /// on every replica.
    RegisterOrigin {
        reg: PeerRegistration,
        now: u64,
    },
    /// A proxy's registration, as [`WriteRequest::RegisterOrigin`].
    RegisterProxy {
        reg: ProxyRegistration,
        now: u64,
    },
    /// Tombstones `name` in `role`'s registry and frees its address.
    Release {
        role: Role,
        name: String,
    },
    /// [`WriteRequest::Release`] for a lapsed lease: only if the owner's
    /// `last_seen` is still older than `last_seen_before` when the entry
    /// applies, so a re-registration that commits first (in log order) wins.
    Expire {
        role: Role,
        name: String,
        last_seen_before: u64,
    },
    /// Sets `last_seen = now` in the address book; no registry revision.
    Touch {
        role: Role,
        name: String,
        now: u64,
    },
    /// Records the cluster's tunnel network (CIDR string; `None` =
    /// pin-only) once, before the first registry write.
    SetTunnelNetwork(Option<String>),
    /// The pre-HA registrations of one node, adopted once as the cluster's
    /// initial registry state (see [`import`]).
    Import(Box<import::ImportContent>),
}

/// What applying one entry did — the HTTP layer maps it back to the status
/// codes and bodies the non-HA handlers give.
///
/// A replayed entry (every step it would take already absorbed after a
/// crash) answers `Revision(None)`: openraft only routes responses of
/// entries proposed in the live term, so nobody receives it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WriteResponse {
    /// Config, intent and promote entries. `None` only for a Raft-internal
    /// entry (blank leader no-op, membership change) that never came from
    /// `client_write`, or a replay.
    Revision(Option<u64>),
    /// A registration was stored at `revision` with tunnel `address`.
    Registered { revision: u64, address: IpAddr },
    /// A registration's tombstone was stored at `revision`; `address` is
    /// the tunnel address freed, if it held one.
    Released {
        revision: u64,
        address: Option<IpAddr>,
    },
    /// A deterministic refusal (`409` / `422` / `503` as
    /// `crate::addresses::api::claim_error_response` maps it).
    Rejected(Rejection),
    /// An `Expire` found the owner seen again since its cutoff; nothing
    /// changed.
    NotExpired,
    /// A release or touch of a name unknown to the registry and the book.
    NotFound,
    /// A touch refreshed the owner's `last_seen`.
    Touched,
    /// `SetTunnelNetwork` was applied: the network is recorded (now, or by
    /// an earlier entry — it is recorded once).
    Recorded,
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

/// The log length (entries since the last snapshot) at which production
/// builds a snapshot and lets `openraft` purge the log — `openraft`'s own
/// default.
pub const SNAPSHOT_AFTER: u64 = 5000;

/// This tier's `openraft` config. Election timing is relaxed from the
/// library defaults (150/300/50ms): this is a control-plane group on plain
/// HTTP over `reqwest`, not a low-latency data-path link, so a wider
/// election window trades a slightly slower failover for fewer spurious
/// elections under ordinary scheduling/network jitter in a test or a
/// loaded host. A snapshot is built every `snapshot_after` log entries
/// ([`SNAPSHOT_AFTER`] in production; tests lower it to exercise the
/// purge path), after which `openraft` purges the log and catches a lagging
/// follower or learner up by snapshot. `max_in_snapshot_log_to_keep` and
/// `replication_lag_threshold` stay at their defaults.
///
/// A snapshot travels as JSON, where each byte of its content costs several
/// characters: it is sent in 256 KiB chunks so a chunk stays well under the
/// `/raft/*` routes' body limit, and each chunk gets 30 s instead of
/// `openraft`'s 200 ms default, which a catch-up of a real registry never
/// meets.
#[allow(deprecated)] // `snapshot_max_chunk_size` is how chunks are sized in 0.9
pub fn raft_config(snapshot_after: u64) -> openraft::Config {
    openraft::Config {
        heartbeat_interval: 250,
        election_timeout_min: 800,
        election_timeout_max: 1500,
        snapshot_policy: openraft::SnapshotPolicy::LogsSinceLast(snapshot_after),
        snapshot_max_chunk_size: 256 * 1024,
        install_snapshot_timeout: 30_000,
        ..Default::default()
    }
}

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
    /// Peer-only shared secret gating `/raft/*` — a separate secret from
    /// `--auth-token` (client-facing) and `--parent-token`
    /// (slave-to-parent), matching every other cross-service credential
    /// pair in this fleet.
    pub ha_token: Option<Arc<str>>,
    /// Carries writes this replica forwards to the leader
    /// ([`client::forward_client`]): shared, and bounded by
    /// [`client::FORWARD_TIMEOUT`].
    pub forward: reqwest::Client,
    /// This node's own set-aside pre-HA data, which `/raft/whoami` reports
    /// and `/raft/pre-ha` serves ([`import`]).
    pub pre_ha: import::LocalPreHa,
}

impl HaHandle {
    /// Whether this node is currently the Raft leader.
    pub fn is_leader(&self) -> bool {
        self.raft.metrics().borrow().current_leader == Some(self.node_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ha_join_and_ha_peers_together_are_refused() {
        let e = check_flags(true, true, Some(1)).unwrap_err();
        assert!(e.contains("--ha-join") && e.contains("--ha-peers"), "{e}");
        assert!(check_flags(true, false, Some(1)).is_ok());
        assert!(check_flags(false, true, Some(4)).is_ok());
        assert!(check_flags(false, false, None).is_ok());
    }

    #[test]
    fn ha_needs_a_node_id() {
        assert!(check_flags(true, false, None)
            .unwrap_err()
            .contains("--ha-node-id"));
        assert!(check_flags(false, true, None)
            .unwrap_err()
            .contains("--ha-join requires"));
    }

    #[test]
    fn ha_without_a_token_is_refused() {
        let e = check_ha_token(true, None).unwrap_err();
        assert!(e.contains("--ha-token"), "{e}");
        assert!(check_ha_token(false, None).is_ok());
    }

    #[test]
    fn short_ha_token_is_refused_whether_or_not_ha_is_on() {
        assert!(check_ha_token(true, Some("short")).is_err());
        assert!(check_ha_token(false, Some("short")).is_err());
        assert!(check_ha_token(true, Some("0123456789abcdef")).is_ok());
    }

    #[test]
    fn production_raft_config_keeps_openraft_snapshot_defaults() {
        let config = raft_config(SNAPSHOT_AFTER).validate().unwrap();
        let defaults = openraft::Config::default();
        assert_eq!(config.snapshot_policy, defaults.snapshot_policy);
        assert_eq!(
            config.max_in_snapshot_log_to_keep,
            defaults.max_in_snapshot_log_to_keep
        );
        assert_eq!(
            config.replication_lag_threshold,
            defaults.replication_lag_threshold
        );
    }

    #[test]
    fn a_lower_snapshot_threshold_is_honoured() {
        let config = raft_config(10).validate().unwrap();
        assert_eq!(
            config.snapshot_policy,
            openraft::SnapshotPolicy::LogsSinceLast(10)
        );
    }
}
