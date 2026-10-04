//! `/admin/ha/members` — live membership of the tier's Raft group ("Live
//! membership",
//! `docs/superpowers/specs/2026-10-03-ha-replicated-address-allocation-design.md`).
//!
//! Gated by `--auth-token` like `/admin/adopt`. `GET` is served from this
//! node's own Raft metrics; the writes (`POST` add, `DELETE` remove, `PUT`
//! re-address) run on the leader — a follower forwards them there, `X-Actor`
//! and `Authorization` included — and are logged at `info` with their actor.
//!
//! Before an add or a re-address the leader asks the node at the given address
//! who it is (`GET /raft/whoami`, behind `--ha-token`): `openraft` warns that
//! pointing a node id at another node's address can produce two leaders, so
//! the change is refused unless the answer carries the expected id, and an
//! add additionally needs an empty log (or an existing learner of this
//! cluster).

// The helpers return the `Response` to send as their `Err`, like the handlers
// they serve; boxing it would only move the size onto every call site.
#![allow(clippy::result_large_err)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get};
use axum::{Json, Router};
use openraft::error::{ChangeMembershipError, ClientWriteError, RaftError};
use openraft::{BasicNode, ChangeMembers};
use serde::{Deserialize, Serialize};

use super::peers::peer_url;
use super::routes::Whoami;
use super::{HaHandle, NodeId};

/// How long a forwarded `POST` (an add) may take: the leader waits for the
/// new node to catch up, by log or by snapshot, which can far outlast the
/// 10 s [`super::client::FORWARD_TIMEOUT`] of an ordinary write.
pub const MEMBER_ADD_TIMEOUT: Duration = Duration::from_secs(300);

/// Marks a request a follower already forwarded, so the leader never
/// forwards it a second time if its own view of the leader has just changed.
const FORWARDED: &str = "x-gsp-forwarded";

#[derive(Clone)]
struct MembersState {
    ha: Arc<HaHandle>,
    /// Same posture as `/admin/adopt`: `None` leaves the routes open.
    auth_token: Option<Arc<str>>,
    /// Carries a forwarded add, bounded by [`MEMBER_ADD_TIMEOUT`].
    add_client: reqwest::Client,
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
    /// This node's own Raft log, for operators and the fleet tests: a node
    /// that caught up by snapshot has `purged_index` set.
    last_log_index: Option<u64>,
    purged_index: Option<u64>,
}

pub fn router(ha: Arc<HaHandle>, auth_token: Option<Arc<str>>) -> Router {
    let state = MembersState {
        ha,
        auth_token,
        add_client: super::client::forward_client(MEMBER_ADD_TIMEOUT),
    };
    Router::new()
        .route("/admin/ha/members", get(list).post(add))
        .route("/admin/ha/members/{id}", delete(remove).put(readdress))
        .route_layer(axum::middleware::from_fn_with_state(
            gsp_http::server::BearerAuth::new(state.auth_token.as_deref()),
            gsp_http::server::require_bearer,
        ))
        .with_state(state)
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(ErrorBody {
            error: message.into(),
        }),
    )
        .into_response()
}

/// Why a membership change is refused before `openraft` sees it.
#[derive(Debug, PartialEq, Eq)]
pub enum MemberError {
    /// The address did not answer `/raft/whoami` (`503`).
    Unreachable { addr: String, reason: String },
    /// The address answered with another node id (`422`).
    IdMismatch {
        addr: String,
        expected: NodeId,
        found: NodeId,
    },
    /// An add of a node whose log is not empty and that is no learner here
    /// (`422`).
    ForeignLog { addr: String, id: NodeId },
}

impl MemberError {
    fn into_response(self) -> Response {
        match self {
            MemberError::Unreachable { addr, reason } => error(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("node at {addr} did not answer /raft/whoami: {reason}"),
            ),
            MemberError::IdMismatch {
                addr,
                expected,
                found,
            } => error(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("{addr} answers as node {found}, not node {expected}; refusing the change"),
            ),
            MemberError::ForeignLog { addr, id } => error(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!(
                    "node {id} at {addr} already has a Raft log and is not a learner of this \
                     cluster; start it with empty data and --ha-join"
                ),
            ),
        }
    }
}

/// `GET <addr>/raft/whoami`.
pub(super) async fn fetch_whoami(
    client: &reqwest::Client,
    ha_token: Option<&str>,
    addr: &str,
) -> Result<Whoami, String> {
    let mut req = client.get(peer_url(addr, "/raft/whoami"));
    if let Some(token) = ha_token {
        req = req.bearer_auth(token);
    }
    let resp = req.send().await.map_err(|e| gsp_http::error_chain(&e))?;
    if !resp.status().is_success() {
        return Err(format!("answered {}", resp.status()));
    }
    resp.json().await.map_err(|e| e.to_string())
}

/// Checks the node at `addr` is node `id` (and, with `for_add`, that its log
/// is empty) before the membership names it.
pub async fn verify_identity(
    client: &reqwest::Client,
    addr: &str,
    id: NodeId,
    ha_token: Option<&str>,
    for_add: bool,
) -> Result<(), MemberError> {
    let who = fetch_whoami(client, ha_token, addr)
        .await
        .map_err(|reason| MemberError::Unreachable {
            addr: addr.into(),
            reason,
        })?;
    if who.node_id != id {
        return Err(MemberError::IdMismatch {
            addr: addr.into(),
            expected: id,
            found: who.node_id,
        });
    }
    if for_add && !who.log_empty {
        return Err(MemberError::ForeignLog {
            addr: addr.into(),
            id,
        });
    }
    Ok(())
}

/// The timeout a forwarded `method` request is allowed.
fn forward_timeout(method: &Method) -> Duration {
    if method == Method::POST {
        MEMBER_ADD_TIMEOUT
    } else {
        super::client::FORWARD_TIMEOUT
    }
}

struct Members {
    voters: BTreeMap<NodeId, String>,
    learners: BTreeMap<NodeId, String>,
}

fn members(ha: &HaHandle) -> (Members, openraft::RaftMetrics<NodeId, BasicNode>) {
    let metrics = ha.raft.metrics().borrow().clone();
    let membership = metrics.membership_config.membership();
    let voter_ids: BTreeSet<NodeId> = membership.voter_ids().collect();
    let mut out = Members {
        voters: BTreeMap::new(),
        learners: BTreeMap::new(),
    };
    for (id, node) in membership.nodes() {
        let side = if voter_ids.contains(id) {
            &mut out.voters
        } else {
            &mut out.learners
        };
        side.insert(*id, node.addr.clone());
    }
    (out, metrics)
}

async fn list(State(state): State<MembersState>) -> Response {
    let (members, metrics) = members(&state.ha);
    let side = |m: BTreeMap<NodeId, String>| {
        m.into_iter()
            .map(|(id, addr)| Member { id, addr })
            .collect()
    };
    Json(MembersOut {
        node_id: state.ha.node_id,
        leader: metrics.current_leader,
        voters: side(members.voters),
        learners: side(members.learners),
        last_log_index: metrics.last_log_index,
        purged_index: metrics.purged.map(|p| p.index),
    })
    .into_response()
}

/// `Ok(())` when this node is the leader and should act; `Err` carries the
/// response of a forward to the leader (or why there is none).
async fn on_leader(
    state: &MembersState,
    method: Method,
    path: String,
    headers: &HeaderMap,
    body: String,
) -> Result<(), Response> {
    let (members, metrics) = members(&state.ha);
    let me = state.ha.node_id;
    let Some(leader) = metrics.current_leader else {
        return Err(error(
            StatusCode::SERVICE_UNAVAILABLE,
            "no raft leader elected yet; retry shortly",
        ));
    };
    if leader == me {
        return Ok(());
    }
    if headers.contains_key(FORWARDED) {
        return Err(error(
            StatusCode::SERVICE_UNAVAILABLE,
            "the leader changed while the request was forwarded; retry shortly",
        ));
    }
    let Some(addr) = members
        .voters
        .get(&leader)
        .or_else(|| members.learners.get(&leader))
    else {
        return Err(error(
            StatusCode::SERVICE_UNAVAILABLE,
            "the leader is not in this node's membership yet; retry shortly",
        ));
    };
    let client = if method == Method::POST {
        &state.add_client
    } else {
        &state.ha.forward
    };
    let mut req = client
        .request(method.clone(), peer_url(addr, &path))
        .header(FORWARDED, "1")
        .body(body);
    for name in [header::AUTHORIZATION.as_str(), "x-actor", "content-type"] {
        if let Some(value) = headers.get(name) {
            req = req.header(name, value.as_bytes());
        }
    }
    let relayed = match req.send().await {
        Ok(resp) => {
            let status = resp.status();
            resp.text().await.map(|text| (status, text))
        }
        Err(e) => Err(e),
    };
    Err(match relayed {
        Ok((status, text)) => (
            StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
            [(header::CONTENT_TYPE, "application/json")],
            text,
        )
            .into_response(),
        Err(e) if e.is_timeout() => error(
            StatusCode::GATEWAY_TIMEOUT,
            format!(
                "the leader did not answer within {:?}; the change may still complete, check \
                 GET /admin/ha/members before retrying",
                forward_timeout(&method)
            ),
        ),
        Err(e) => {
            tracing::warn!(error = %gsp_http::error_chain(&e), "forwarding a membership change to the leader failed");
            error(
                StatusCode::SERVICE_UNAVAILABLE,
                "could not reach the current leader; retry shortly",
            )
        }
    })
}

fn actor(headers: &HeaderMap) -> &str {
    headers
        .get("x-actor")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-")
}

/// A `raft` error as the response it deserves.
fn raft_error(e: RaftError<NodeId, ClientWriteError<NodeId, BasicNode>>) -> Response {
    match e {
        RaftError::APIError(ClientWriteError::ForwardToLeader(_)) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "this node is no longer the leader; retry shortly",
        ),
        RaftError::APIError(ClientWriteError::ChangeMembershipError(
            ChangeMembershipError::InProgress(_),
        )) => error(
            StatusCode::CONFLICT,
            "another membership change is in progress; retry shortly",
        ),
        RaftError::APIError(ClientWriteError::ChangeMembershipError(e)) => {
            error(StatusCode::UNPROCESSABLE_ENTITY, e.to_string())
        }
        RaftError::Fatal(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Deserialize)]
struct AddBody {
    id: NodeId,
    addr: String,
}

#[derive(Deserialize)]
struct AddrBody {
    addr: String,
}

fn parse<T: serde::de::DeserializeOwned>(body: &str) -> Result<T, Response> {
    serde_json::from_str(body).map_err(|e| error(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()))
}

fn check_addr(addr: &str) -> Result<(), Response> {
    if addr.trim().is_empty() {
        return Err(error(StatusCode::UNPROCESSABLE_ENTITY, "addr is empty"));
    }
    Ok(())
}

async fn add(State(state): State<MembersState>, headers: HeaderMap, body: String) -> Response {
    let path = "/admin/ha/members".to_string();
    if let Err(r) = on_leader(&state, Method::POST, path, &headers, body.clone()).await {
        return r;
    }
    let req: AddBody = match parse(&body) {
        Ok(r) => r,
        Err(r) => return r,
    };
    if let Err(r) = check_addr(&req.addr) {
        return r;
    }
    let ha = &state.ha;
    let (members, _) = members(ha);
    if members.voters.contains_key(&req.id) {
        return error(
            StatusCode::CONFLICT,
            format!("node {} is already a voter", req.id),
        );
    }
    let already_learner = members.learners.contains_key(&req.id);
    if let Err(e) = verify_identity(
        &ha.forward,
        &req.addr,
        req.id,
        ha.ha_token.as_deref(),
        !already_learner,
    )
    .await
    {
        return e.into_response();
    }
    super::peers::warn_if_plain_remote(&req.addr);
    tracing::info!(actor = actor(&headers), id = req.id, addr = %req.addr, "adding an HA member");
    // Blocking: returns once the learner has caught up, by log or snapshot.
    if let Err(e) = ha
        .raft
        .add_learner(req.id, BasicNode::new(req.addr.clone()), true)
        .await
    {
        return raft_error(e);
    }
    match ha
        .raft
        .change_membership(ChangeMembers::AddVoterIds(BTreeSet::from([req.id])), false)
        .await
    {
        Ok(_) => list(State(state)).await,
        Err(e) => raft_error(e),
    }
}

async fn remove(
    State(state): State<MembersState>,
    Path(id): Path<NodeId>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/admin/ha/members/{id}");
    if let Err(r) = on_leader(&state, Method::DELETE, path, &headers, String::new()).await {
        return r;
    }
    let ha = &state.ha;
    let (members, _) = members(ha);
    let change = if members.voters.contains_key(&id) {
        if members.voters.len() == 1 {
            return error(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("node {id} is the last voter; the cluster cannot lose it"),
            );
        }
        ChangeMembers::RemoveVoters(BTreeSet::from([id]))
    } else if members.learners.contains_key(&id) {
        ChangeMembers::RemoveNodes(BTreeSet::from([id]))
    } else {
        return error(StatusCode::NOT_FOUND, format!("node {id} is not a member"));
    };
    tracing::info!(actor = actor(&headers), id, "removing an HA member");
    // `retain = false`: the node leaves the membership entirely. Removing the
    // leader is allowed; `openraft` steps it down once the change commits.
    match ha.raft.change_membership(change, false).await {
        Ok(_) => list(State(state)).await,
        Err(e) => raft_error(e),
    }
}

async fn readdress(
    State(state): State<MembersState>,
    Path(id): Path<NodeId>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let path = format!("/admin/ha/members/{id}");
    if let Err(r) = on_leader(&state, Method::PUT, path, &headers, body.clone()).await {
        return r;
    }
    let req: AddrBody = match parse(&body) {
        Ok(r) => r,
        Err(r) => return r,
    };
    if let Err(r) = check_addr(&req.addr) {
        return r;
    }
    let ha = &state.ha;
    let (members, _) = members(ha);
    if !members.voters.contains_key(&id) && !members.learners.contains_key(&id) {
        return error(StatusCode::NOT_FOUND, format!("node {id} is not a member"));
    }
    if let Err(e) = verify_identity(&ha.forward, &req.addr, id, ha.ha_token.as_deref(), false).await
    {
        return e.into_response();
    }
    super::peers::warn_if_plain_remote(&req.addr);
    tracing::info!(actor = actor(&headers), id, addr = %req.addr, "re-addressing an HA member");
    let nodes = BTreeMap::from([(id, BasicNode::new(req.addr))]);
    match ha
        .raft
        .change_membership(ChangeMembers::SetNodes(nodes), false)
        .await
    {
        Ok(_) => list(State(state)).await,
        Err(e) => raft_error(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ha::routes::Whoami;
    use crate::ha::test_support::single_node;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use tower::ServiceExt;

    /// A node that answers `/raft/whoami` as `node_id` with `log_empty`.
    async fn stub_whoami(node_id: NodeId, log_empty: bool) -> String {
        let app = Router::new().route(
            "/raft/whoami",
            get(move || async move {
                Json(Whoami {
                    node_id,
                    log_empty,
                    pre_ha: Default::default(),
                })
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        addr
    }

    async fn call(
        app: &Router,
        method: &str,
        path: &str,
        body: serde_json::Value,
    ) -> (StatusCode, String) {
        let resp = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .method(method)
                    .uri(path)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    async fn app() -> (Router, tempfile::TempDir) {
        let (ha, _cluster, dir) = single_node(1, "127.0.0.1:1").await;
        (router(ha, None), dir)
    }

    #[tokio::test]
    async fn get_lists_the_voters_and_the_leader() {
        let (app, _dir) = app().await;
        let (status, body) = call(&app, "GET", "/admin/ha/members", serde_json::json!({})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["leader"], 1);
        assert_eq!(v["voters"][0]["id"], 1);
        assert_eq!(v["voters"][0]["addr"], "127.0.0.1:1");
        assert_eq!(v["learners"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn add_refuses_an_identity_mismatch() {
        let (app, _dir) = app().await;
        let addr = stub_whoami(7, true).await;
        let (status, body) = call(
            &app,
            "POST",
            "/admin/ha/members",
            serde_json::json!({"id": 4, "addr": addr}),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert!(body.contains("node 7") && body.contains("node 4"), "{body}");
    }

    #[tokio::test]
    async fn add_refuses_a_node_with_a_foreign_log() {
        let (app, _dir) = app().await;
        let addr = stub_whoami(4, false).await;
        let (status, body) = call(
            &app,
            "POST",
            "/admin/ha/members",
            serde_json::json!({"id": 4, "addr": addr}),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert!(body.contains("--ha-join"), "{body}");
    }

    #[tokio::test]
    async fn add_of_an_unreachable_address_is_503_and_of_a_voter_is_409() {
        let (app, _dir) = app().await;
        let (status, _) = call(
            &app,
            "POST",
            "/admin/ha/members",
            serde_json::json!({"id": 4, "addr": "127.0.0.1:1"}),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let (status, body) = call(
            &app,
            "POST",
            "/admin/ha/members",
            serde_json::json!({"id": 1, "addr": "127.0.0.1:1"}),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
    }

    #[tokio::test]
    async fn delete_refuses_the_last_voter_and_an_unknown_id() {
        let (app, _dir) = app().await;
        let (status, body) =
            call(&app, "DELETE", "/admin/ha/members/1", serde_json::json!({})).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert!(body.contains("last voter"), "{body}");
        let (status, _) = call(&app, "DELETE", "/admin/ha/members/9", serde_json::json!({})).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn put_refuses_an_address_that_answers_with_another_id() {
        let (app, _dir) = app().await;
        let addr = stub_whoami(2, false).await;
        let (status, body) = call(
            &app,
            "PUT",
            "/admin/ha/members/1",
            serde_json::json!({"addr": addr}),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert!(body.contains("node 2") && body.contains("node 1"), "{body}");
        let (status, _) = call(
            &app,
            "PUT",
            "/admin/ha/members/9",
            serde_json::json!({"addr": addr}),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn put_re_addresses_a_member_whose_new_address_answers_as_it() {
        let (app, _dir) = app().await;
        let addr = stub_whoami(1, false).await;
        let (status, body) = call(
            &app,
            "PUT",
            "/admin/ha/members/1",
            serde_json::json!({"addr": addr}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["voters"][0]["addr"], addr.as_str());
    }

    #[tokio::test]
    async fn the_routes_need_the_auth_token_when_one_is_set() {
        let (ha, _cluster, _dir) = single_node(1, "127.0.0.1:1").await;
        let app = router(ha, Some(Arc::from("s3cret")));
        let (status, _) = call(&app, "GET", "/admin/ha/members", serde_json::json!({})).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn a_forwarded_member_add_uses_the_long_timeout() {
        assert_eq!(forward_timeout(&Method::POST), MEMBER_ADD_TIMEOUT);
        assert_eq!(
            forward_timeout(&Method::DELETE),
            crate::ha::client::FORWARD_TIMEOUT
        );
        assert_eq!(
            forward_timeout(&Method::PUT),
            crate::ha::client::FORWARD_TIMEOUT
        );
        assert!(MEMBER_ADD_TIMEOUT > crate::ha::client::FORWARD_TIMEOUT);
    }
}
