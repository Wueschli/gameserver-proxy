//! `/raft/append`, `/raft/vote`, `/raft/snapshot` — the inbound side of
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
        Some(token) if token == expected => next.run(req).await,
        _ => (StatusCode::UNAUTHORIZED, "unauthorized").into_response(),
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
