//! `/raft/append`, `/raft/vote`, `/raft/snapshot` (plus `/raft/whoami` and
//! `/raft/pre-ha`, read-only) — the inbound side of
//! `crate::ha::network`'s outbound calls. Gated by the same peer-only
//! `--ha-token` `crate::ha::HaHandle` carries, via a small `require_bearer`
//! mirroring every other one in this crate (see
//! `crate::intent::api::require_bearer`'s doc comment for why each `axum`
//! state gets its own copy rather than a shared generic).

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use openraft::raft::{AppendEntriesRequest, InstallSnapshotRequest, VoteRequest};

use super::{HaHandle, NodeId};

pub fn router(ha: Arc<HaHandle>) -> Router {
    Router::new()
        .route("/raft/append", post(append))
        .route("/raft/vote", post(vote))
        .route("/raft/snapshot", post(snapshot))
        .route("/raft/whoami", get(whoami))
        .route("/raft/pre-ha", get(pre_ha))
        .route_layer(axum::middleware::from_fn_with_state(
            ha.clone(),
            require_bearer,
        ))
        .with_state(ha)
}

async fn require_bearer(State(ha): State<Arc<HaHandle>>, req: Request, next: Next) -> Response {
    let Some(expected) = ha.ha_token.as_deref() else {
        return next.run(req).await;
    };
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    match presented {
        Some(token) if gsp_http::token_eq(token, expected) => next.run(req).await,
        _ => (StatusCode::UNAUTHORIZED, "unauthorized").into_response(),
    }
}

/// Who this node is and what it holds: the leader of a fresh cluster asks every
/// voter before choosing the pre-HA data to import (`ha::init`), and member
/// changes check an address answers with the id they expect.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Whoami {
    pub node_id: NodeId,
    /// No Raft log entry yet (a node that has never been part of a cluster).
    pub log_empty: bool,
    pub pre_ha: super::import::PreHaSummary,
}

async fn whoami(State(ha): State<Arc<HaHandle>>) -> impl IntoResponse {
    Json(Whoami {
        node_id: ha.node_id,
        log_empty: ha.raft.metrics().borrow().last_log_index.is_none(),
        pre_ha: ha.pre_ha.summary,
    })
}

/// This node's set-aside pre-HA data, `404` when it has none.
async fn pre_ha(State(ha): State<Arc<HaHandle>>) -> Response {
    let local = ha.pre_ha.clone();
    let node_id = ha.node_id;
    let read = tokio::task::spawn_blocking(move || {
        super::import::read_pre_ha(&local.data_dir, local.network, node_id)
    })
    .await;
    match read {
        Ok(Ok(Some(content))) => Json(content).into_response(),
        Ok(Ok(None)) => StatusCode::NOT_FOUND.into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
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
