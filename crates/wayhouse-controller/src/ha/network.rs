//! `RaftNetworkFactory` + `RaftNetwork` over `reqwest`, posting to a peer's
//! `/raft/*` routes — adapted from `openraft`'s own `raft-kv-memstore`
//! example (`actix-web` there, `axum`/`reqwest` here; every other
//! cross-service RPC in this fleet already uses `reqwest`, so this is
//! consistent with the rest of the crate, not a new pattern).

use openraft::error::{InstallSnapshotError, NetworkError, RemoteError, Unreachable};
use openraft::network::{RPCOption, RaftNetwork, RaftNetworkFactory};
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest, InstallSnapshotResponse,
    VoteRequest, VoteResponse,
};
use openraft::BasicNode;
use serde::de::DeserializeOwned;
use serde::Serialize;

use super::{typ, NodeId};

#[derive(Clone)]
pub struct Network {
    /// Peer-only shared secret presented on every outbound `/raft/*` call —
    /// the same `--ha-token` this node also checks on inbound calls
    /// (`crate::ha::routes`).
    pub ha_token: Option<std::sync::Arc<str>>,
    /// One client for every peer, so its pool keeps each peer's connection
    /// open between RPCs; a client per RPC would redo the TCP (and, for
    /// `https://` peers, TLS) handshake on every 250 ms heartbeat. It sets
    /// no timeout of its own: openraft bounds every RPC (heartbeat, vote,
    /// snapshot) from outside, so a stuck pooled connection costs one RPC.
    client: reqwest::Client,
}

impl Network {
    /// Build after `--ca-file` is loaded: the client captures the roots.
    pub fn new(ha_token: Option<std::sync::Arc<str>>) -> Self {
        Self {
            ha_token,
            client: wayhouse_http::client(),
        }
    }
}

impl Network {
    // `RPCError`'s size is dictated by `openraft` itself (it embeds a whole
    // `RaftError`); boxing it would just move the "large" complaint onto
    // every call site that has to unbox it again, for a control-plane RPC
    // helper that's never called per-connection.
    #[allow(clippy::result_large_err)]
    async fn send_rpc<Req, Resp, Err>(
        &self,
        target: NodeId,
        target_node: &BasicNode,
        path: &str,
        req: Req,
    ) -> Result<Resp, openraft::error::RPCError<NodeId, BasicNode, Err>>
    where
        Req: Serialize,
        Err: std::error::Error + DeserializeOwned,
        Resp: DeserializeOwned,
    {
        let url = super::peers::peer_url(&target_node.addr, path);
        let mut builder = self.client.post(&url).json(&req);
        if let Some(token) = &self.ha_token {
            builder = builder.bearer_auth(token);
        }
        let resp = builder.send().await.map_err(|e| {
            if e.is_connect() {
                openraft::error::RPCError::Unreachable(Unreachable::new(&e))
            } else {
                openraft::error::RPCError::Network(NetworkError::new(&e))
            }
        })?;
        let result: Result<Resp, Err> = resp
            .json()
            .await
            .map_err(|e| openraft::error::RPCError::Network(NetworkError::new(&e)))?;
        result.map_err(|e| openraft::error::RPCError::RemoteError(RemoteError::new(target, e)))
    }
}

impl RaftNetworkFactory<super::TypeConfig> for Network {
    type Network = NetworkConnection;

    async fn new_client(&mut self, target: NodeId, node: &BasicNode) -> Self::Network {
        NetworkConnection {
            owner: self.clone(),
            target,
            target_node: node.clone(),
        }
    }
}

pub struct NetworkConnection {
    owner: Network,
    target: NodeId,
    target_node: BasicNode,
}

impl RaftNetwork<super::TypeConfig> for NetworkConnection {
    async fn append_entries(
        &mut self,
        req: AppendEntriesRequest<super::TypeConfig>,
        _option: RPCOption,
    ) -> Result<AppendEntriesResponse<NodeId>, typ::RPCError> {
        self.owner
            .send_rpc(self.target, &self.target_node, "/raft/append", req)
            .await
    }

    async fn install_snapshot(
        &mut self,
        req: InstallSnapshotRequest<super::TypeConfig>,
        _option: RPCOption,
    ) -> Result<InstallSnapshotResponse<NodeId>, typ::RPCError<InstallSnapshotError>> {
        self.owner
            .send_rpc(self.target, &self.target_node, "/raft/snapshot", req)
            .await
    }

    async fn vote(
        &mut self,
        req: VoteRequest<NodeId>,
        _option: RPCOption,
    ) -> Result<VoteResponse<NodeId>, typ::RPCError> {
        self.owner
            .send_rpc(self.target, &self.target_node, "/raft/vote", req)
            .await
    }
}
