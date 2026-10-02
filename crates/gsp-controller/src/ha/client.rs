//! [`propose_write`] — the one function `api::submit` and
//! `intent::api::submit_intent` call instead of `apply_revision` directly
//! when HA is on. Proposes a Raft write on this replica; if this replica
//! isn't the leader, **transparently forwards the original request to the
//! current leader** (over HTTP or HTTPS, per its `--ha-peers` entry) rather than returning a redirect — see
//! `docs/10` "Intra-tier HA (design)" for why: every existing client of
//! this API (`gsp`, `gsp-ui`, `curl`) stays completely unaware HA exists.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use super::{typ, HaHandle, WriteRequest};

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

/// Proposes `req` via Raft. `path` is this write's own route (`/config` or
/// `/intent`) — used only to forward `body` to the leader's copy of the same
/// route if this replica isn't it; `actor` (phase 12 slice 8's `X-Actor`, if
/// any) rides along on that forward so the leader's own handler — which
/// re-parses it independently, exactly as if the browser/`gsp`/`curl` had
/// called the leader directly — attributes the write to the same actor.
/// Returns the committed revision on success (always `Some` for a real
/// `WriteRequest`).
pub async fn propose_write(
    ha: &HaHandle,
    req: WriteRequest,
    path: &str,
    body: String,
    actor: Option<&str>,
) -> Response {
    match ha.raft.client_write(req).await {
        Ok(resp) => {
            let revision = resp.response().revision;
            (
                StatusCode::OK,
                axum::Json(serde_json::json!({ "revision": revision })),
            )
                .into_response()
        }
        Err(e) => handle_write_error(ha, e, path, body, actor).await,
    }
}

async fn handle_write_error(
    ha: &HaHandle,
    err: typ::RaftError<typ::ClientWriteError>,
    path: &str,
    body: String,
    actor: Option<&str>,
) -> Response {
    let openraft::error::RaftError::APIError(api_err) = err else {
        return service_unavailable("raft internal error; retry shortly");
    };

    let (leader_id, leader_node) = match api_err {
        openraft::error::ClientWriteError::ForwardToLeader(fwd) => (fwd.leader_id, fwd.leader_node),
        openraft::error::ClientWriteError::ChangeMembershipError(_) => (None, None),
    };

    match forward_target(leader_id, leader_node, ha.node_id) {
        Ok(leader) => forward_to_leader(&leader.addr, path, body, actor).await,
        Err(msg) => service_unavailable(msg),
    }
}

/// Where a write this replica can't commit should go, or why nowhere. Decided
/// by node id, not address: a leader naming *this* node (transiently, right
/// after an election) must never be forwarded to, even if this node's
/// `--ha-peers` entry no longer matches its address in the Raft membership.
fn forward_target(
    leader_id: Option<super::NodeId>,
    leader_node: Option<openraft::BasicNode>,
    self_id: super::NodeId,
) -> Result<openraft::BasicNode, &'static str> {
    if leader_id == Some(self_id) {
        return Err("this replica just lost leadership; retry shortly");
    }
    leader_node.ok_or("no raft leader elected yet; retry shortly")
}

async fn forward_to_leader(
    leader_addr: &str,
    path: &str,
    body: String,
    actor: Option<&str>,
) -> Response {
    let url = super::peers::peer_url(leader_addr, path);
    let mut req = gsp_http::client().post(&url).body(body);
    if let Some(actor) = actor {
        req = req.header("X-Actor", actor);
    }
    match req.send().await {
        Ok(resp) => {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            (
                axum::http::StatusCode::from_u16(status.as_u16())
                    .unwrap_or(StatusCode::BAD_GATEWAY),
                text,
            )
                .into_response()
        }
        Err(e) => {
            tracing::warn!(error = %gsp_http::error_chain(&e), %leader_addr, "forwarding a write to the raft leader failed");
            service_unavailable("could not reach the current raft leader; retry shortly")
        }
    }
}

fn service_unavailable(msg: &str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        axum::Json(ErrorBody { error: msg.into() }),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use openraft::BasicNode;

    #[test]
    fn a_leader_naming_this_node_is_never_forwarded_to_even_under_another_address() {
        // The membership address can differ from this node's current
        // --ha-peers entry (edited after bootstrap); the id cannot.
        let got = forward_target(Some(2), Some(BasicNode::new("127.0.0.1:9911")), 2);
        assert!(got.is_err());
    }

    #[test]
    fn another_leader_is_forwarded_to() {
        let got = forward_target(Some(1), Some(BasicNode::new("https://ctl-1:8443")), 2);
        assert_eq!(got.unwrap().addr, "https://ctl-1:8443");
    }

    #[test]
    fn no_known_leader_is_unavailable() {
        assert!(forward_target(None, None, 2).is_err());
    }
}
