//! `/raft/append`, `/raft/vote`, `/raft/snapshot` — the inbound side of
//! `crate::ha::network`'s outbound calls. Gated by the same peer-only
//! `--ha-token` `crate::ha::HaHandle` carries, via a small `require_bearer`
//! mirroring every other one in this crate (see
//! `crate::intent::api::require_bearer`'s doc comment for why each `axum`
//! state gets its own copy rather than a shared generic).

use std::sync::Arc;

use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use openraft::raft::{AppendEntriesRequest, InstallSnapshotRequest, VoteRequest};

use super::{HaHandle, NodeId};

pub fn router(ha: Arc<HaHandle>) -> Router {
    Router::new()
        .route("/raft/append", post(append))
        .route("/raft/vote", post(vote))
        .route("/raft/snapshot", post(snapshot))
        .route_layer(axum::middleware::from_fn_with_state(
            gsp_http::server::BearerAuth::new(ha.ha_token.as_deref()),
            gsp_http::server::require_bearer,
        ))
        .with_state(ha)
}

async fn vote(
    State(ha): State<Arc<HaHandle>>,
    Json(req): Json<VoteRequest<NodeId>>,
) -> impl IntoResponse {
    Json(ha.raft.vote(req).await)
}

async fn append(
    State(ha): State<Arc<HaHandle>>,
    Json(req): Json<AppendEntriesRequest<super::TypeConfig>>,
) -> impl IntoResponse {
    Json(ha.raft.append_entries(req).await)
}

async fn snapshot(
    State(ha): State<Arc<HaHandle>>,
    Json(req): Json<InstallSnapshotRequest<super::TypeConfig>>,
) -> impl IntoResponse {
    Json(ha.raft.install_snapshot(req).await)
}
