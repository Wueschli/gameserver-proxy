//! `/raft/append`, `/raft/vote`, `/raft/snapshot` (plus `/raft/whoami` and
//! `/raft/pre-ha`, read-only) — the inbound side of
//! `crate::ha::network`'s outbound calls. Gated by the same peer-only
//! `--ha-token` `crate::ha::HaHandle` carries, via the shared
//! [`gsp_http::server::require_bearer`] middleware.

use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use openraft::raft::{AppendEntriesRequest, InstallSnapshotRequest, VoteRequest};

use super::{HaHandle, NodeId};

/// `/raft/vote` carries a term, a node id and a log id.
const MAX_VOTE_BODY: usize = 64 * 1024;
/// `/raft/snapshot` chunks are 256 KiB of bytes (`raft_config`), which JSON
/// writes as a number array, a few times larger.
const MAX_SNAPSHOT_BODY: usize = 4 * 1024 * 1024;
/// `/raft/append` can carry a whole pre-HA import as one entry, so it keeps the
/// big limit.
const MAX_APPEND_BODY: usize = 32 * 1024 * 1024;

pub fn router(ha: Arc<HaHandle>) -> Router {
    Router::new()
        .route(
            "/raft/append",
            post(append).layer(DefaultBodyLimit::max(MAX_APPEND_BODY)),
        )
        .route(
            "/raft/vote",
            post(vote).layer(DefaultBodyLimit::max(MAX_VOTE_BODY)),
        )
        .route(
            "/raft/snapshot",
            post(snapshot).layer(DefaultBodyLimit::max(MAX_SNAPSHOT_BODY)),
        )
        .route("/raft/whoami", get(whoami))
        .route("/raft/pre-ha", get(pre_ha))
        .route_layer(axum::middleware::from_fn_with_state(
            gsp_http::server::BearerAuth::new(ha.ha_token.as_deref()),
            gsp_http::server::require_bearer,
        ))
        .with_state(ha)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ha::test_support::single_node;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn post_bytes(app: &Router, path: &str, len: usize) -> StatusCode {
        let body = format!("\"{}\"", "x".repeat(len));
        app.clone()
            .oneshot(
                Request::post(path)
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn each_raft_route_has_its_own_body_limit() {
        let (ha, _cluster, _dir) = single_node(1, "127.0.0.1:1").await;
        let app = router(ha);
        // Over the limit: refused before the body is parsed.
        for (path, limit) in [
            ("/raft/vote", MAX_VOTE_BODY),
            ("/raft/snapshot", MAX_SNAPSHOT_BODY),
            ("/raft/append", MAX_APPEND_BODY),
        ] {
            assert_eq!(
                post_bytes(&app, path, limit + 1).await,
                StatusCode::PAYLOAD_TOO_LARGE,
                "{path}"
            );
        }
        // Under the limit it is read (and then rejected as malformed JSON for the type).
        assert_eq!(
            post_bytes(&app, "/raft/vote", 1024).await,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
}
