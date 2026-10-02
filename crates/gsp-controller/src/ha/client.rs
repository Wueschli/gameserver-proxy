//! [`propose_write`] — the one function `api::submit` and
//! `intent::api::submit_intent` call instead of `apply_revision` directly
//! when HA is on. Proposes a Raft write on this replica; if this replica
//! isn't the leader, **transparently forwards the original request to the
//! current leader over plain HTTP** rather than returning a redirect — see
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

    let leader_node = match api_err {
        openraft::error::ClientWriteError::ForwardToLeader(fwd) => fwd.leader_node,
        openraft::error::ClientWriteError::ChangeMembershipError(_) => None,
    };

    let Some(leader_node) = leader_node else {
        return service_unavailable("no raft leader elected yet; retry shortly");
    };

    if leader_node.addr == ha.self_addr {
        // A transient self-forward right after an election settles — the
        // leadership info this replica has is a beat behind reality.
        return service_unavailable("this replica just lost leadership; retry shortly");
    }

    forward_to_leader(&leader_node.addr, path, body, actor).await
}

async fn forward_to_leader(
    leader_addr: &str,
    path: &str,
    body: String,
    actor: Option<&str>,
) -> Response {
    let url = format!("http://{leader_addr}{path}");
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
            tracing::warn!(error = %e, %leader_addr, "forwarding a write to the raft leader failed");
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
