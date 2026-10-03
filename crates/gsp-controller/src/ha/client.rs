//! [`propose_write`] — the one function `api::submit`,
//! `intent::api::submit_intent` and the registries' write handlers call
//! instead of writing their stores directly when HA is on. Proposes a Raft
//! write on this replica; if this replica isn't the leader, **transparently
//! forwards the original request to the current leader** (over HTTP or
//! HTTPS, per its `--ha-peers` entry) rather than returning a redirect — see
//! `docs/10` "Intra-tier HA (design)" for why: every existing client of
//! this API (`gsp`, `gsp-ui`, `curl`) stays completely unaware HA exists.
//! The forward is bounded by [`FORWARD_TIMEOUT`], so a half-open leader costs
//! the caller a `504`, never a write handler hung indefinitely.

use std::time::Duration;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use reqwest::Method;
use serde::Serialize;

use super::{typ, HaHandle, WriteRequest, WriteResponse};

/// How long a follower waits for the leader to answer a forwarded write,
/// response body included. A healthy leader commits in milliseconds (a
/// quorum round trip, with 250 ms heartbeats and an election inside 1.5 s),
/// so this only ever fires on a leader that accepted the connection and then
/// went silent; 10 s matches the fleet's other request and handshake bounds.
pub const FORWARD_TIMEOUT: Duration = Duration::from_secs(10);

/// The client [`HaHandle::forward`] holds: one per process, so forwards reuse
/// the leader's pooled connection, with every request bounded by `timeout`.
/// Build after `--ca-file` is loaded: the client captures the roots.
pub fn forward_client(timeout: Duration) -> reqwest::Client {
    gsp_http::builder()
        .timeout(timeout)
        .build()
        .expect("the extra roots were validated by init_ca_file, so the client builds")
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

/// Proposes `req` via Raft and answers with `map` applied to the committed
/// entry's [`WriteResponse`], so each route keeps its own response shape.
/// `path` is this write's own route (`/config`, `/intent`, `/peers`, …) —
/// used only to forward `body` to the leader's copy of the same route (as a
/// `POST`) if this replica isn't it; `actor` (phase 12 slice 8's `X-Actor`, if
/// any) rides along on that forward so the leader's own handler — which
/// re-parses it independently, exactly as if the browser/`gsp`/`curl` had
/// called the leader directly — attributes the write to the same actor.
pub async fn propose_write<F>(
    ha: &HaHandle,
    req: WriteRequest,
    path: &str,
    body: String,
    actor: Option<&str>,
    map: F,
) -> Response
where
    F: FnOnce(WriteResponse) -> Response,
{
    propose_write_as(ha, req, Method::POST, path, body, actor, map).await
}

/// [`propose_write`] for a route whose forward to the leader uses `method`
/// (a registry's `DELETE {base}/{name}`).
pub async fn propose_write_as<F>(
    ha: &HaHandle,
    req: WriteRequest,
    method: Method,
    path: &str,
    body: String,
    actor: Option<&str>,
    map: F,
) -> Response
where
    F: FnOnce(WriteResponse) -> Response,
{
    match ha.raft.client_write(req).await {
        Ok(resp) => map(resp.data),
        Err(e) => handle_write_error(ha, e, method, path, body, actor).await,
    }
}

/// The `{revision}` answer config, intent and promote writes give — the
/// mapper their [`propose_write`] calls pass.
pub fn revision_response(resp: WriteResponse) -> Response {
    // Config, intent and promote entries always answer `Revision`.
    let revision = match resp {
        WriteResponse::Revision(r) => r,
        _ => None,
    };
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({ "revision": revision })),
    )
        .into_response()
}

async fn handle_write_error(
    ha: &HaHandle,
    err: typ::RaftError<typ::ClientWriteError>,
    method: Method,
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
        Ok(leader) => forward_to_leader(&ha.forward, &leader.addr, method, path, body, actor).await,
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
    client: &reqwest::Client,
    leader_addr: &str,
    method: Method,
    path: &str,
    body: String,
    actor: Option<&str>,
) -> Response {
    let url = super::peers::peer_url(leader_addr, path);
    let mut req = client.request(method, &url).body(body);
    if let Some(actor) = actor {
        req = req.header("X-Actor", actor);
    }
    // The client's timeout covers the body too, so read it before relaying.
    let relayed = match req.send().await {
        Ok(resp) => {
            let status = resp.status();
            resp.text().await.map(|text| (status, text))
        }
        Err(e) => Err(e),
    };
    match relayed {
        Ok((status, text)) => (
            StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
            text,
        )
            .into_response(),
        Err(e) if e.is_timeout() => {
            tracing::warn!(error = %gsp_http::error_chain(&e), %leader_addr, "the raft leader did not answer a forwarded write in time");
            error_response(
                StatusCode::GATEWAY_TIMEOUT,
                "the current raft leader did not answer in time; the write may still \
                 have been applied, so check the current revision before retrying",
            )
        }
        Err(e) => {
            tracing::warn!(error = %gsp_http::error_chain(&e), %leader_addr, "forwarding a write to the raft leader failed");
            service_unavailable("could not reach the current raft leader; retry shortly")
        }
    }
}

fn service_unavailable(msg: &str) -> Response {
    error_response(StatusCode::SERVICE_UNAVAILABLE, msg)
}

fn error_response(status: StatusCode, msg: &str) -> Response {
    (status, axum::Json(ErrorBody { error: msg.into() })).into_response()
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

    /// A half-open leader: accepts the connection, reads nothing, never answers.
    async fn hung_leader() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                held.push(stream);
            }
        });
        (addr, task)
    }

    #[tokio::test]
    async fn a_hung_leader_times_out_the_forward_instead_of_hanging_the_handler() {
        let (addr, _leader) = hung_leader().await;
        let client = forward_client(Duration::from_millis(200));
        let resp = tokio::time::timeout(
            Duration::from_secs(5),
            forward_to_leader(
                &client,
                &addr.to_string(),
                Method::POST,
                "/config",
                "{}".into(),
                None,
            ),
        )
        .await
        .expect("the forward must give up on its own, not hang");
        assert_eq!(resp.status(), StatusCode::GATEWAY_TIMEOUT);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&body);
        assert!(body.contains("did not answer in time"), "{body}");
    }
}
