//! `GET /admin/ha/members` — the cluster's voters, learners and leader, served
//! from this node's own Raft metrics. Read-only for now; the write routes
//! (add, remove, re-address) arrive with live membership.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

use super::{HaHandle, NodeId};

#[derive(Clone)]
struct MembersState {
    ha: Arc<HaHandle>,
    /// Same posture as `/admin/adopt`: `None` leaves the route open.
    auth_token: Option<Arc<str>>,
}

#[derive(Serialize)]
struct Member {
    id: NodeId,
    addr: String,
}

#[derive(Serialize)]
struct MembersOut {
    /// This node's own id.
    node_id: NodeId,
    leader: Option<NodeId>,
    voters: Vec<Member>,
    learners: Vec<Member>,
}

pub fn router(ha: Arc<HaHandle>, auth_token: Option<Arc<str>>) -> Router {
    let state = MembersState { ha, auth_token };
    Router::new()
        .route("/admin/ha/members", get(list))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_bearer,
        ))
        .with_state(state)
}

async fn require_bearer(State(state): State<MembersState>, req: Request, next: Next) -> Response {
    let Some(expected) = state.auth_token.as_deref() else {
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

async fn list(State(state): State<MembersState>) -> Response {
    let metrics = state.ha.raft.metrics().borrow().clone();
    let membership = metrics.membership_config.membership();
    let voter_ids: std::collections::BTreeSet<NodeId> = membership.voter_ids().collect();
    let mut voters = Vec::new();
    let mut learners = Vec::new();
    for (id, node) in membership.nodes() {
        let member = Member {
            id: *id,
            addr: node.addr.clone(),
        };
        if voter_ids.contains(id) {
            voters.push(member);
        } else {
            learners.push(member);
        }
    }
    Json(MembersOut {
        node_id: state.ha.node_id,
        leader: metrics.current_leader,
        voters,
        learners,
    })
    .into_response()
}
