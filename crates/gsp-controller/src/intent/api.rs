//! `POST /intent` / `GET /intent/subscribe` — the intent log's HTTP surface.
//! Deliberately a smaller surface than `crate::api`'s config API (no
//! revision history/diff/rollback yet — an intent op is a point-in-time
//! action, not a document worth diffing against another one): just submit
//! and the catch-up-then-tail subscribe `gsp`'s `intent_client` consumes.
//!
//! A `slave` tier never *originates* an intent op locally (the `slave`
//! write gate below), but does hold a relayed copy of its parent's intent
//! log — see [`crate::intent::relay`] (phase 12 slice 4), the intent-log
//! counterpart to [`crate::parent_client`]'s config relay.

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::{Stream, StreamExt};

use super::IntentOp;
use crate::role::{Role, RoleHandle};
use crate::store::{RevisionBytes, Store, StoreError};

const UPDATES_CAPACITY: usize = 64;

#[derive(Clone)]
pub struct IntentState {
    pub store: Arc<Store>,
    pub updates: broadcast::Sender<u64>,
    pub role: RoleHandle,
    /// Bearer token every `/intent*` request must present, or `None` to
    /// leave the API open — same posture as `crate::api::AppState`'s own
    /// `auth_token`, deliberately a separate field/check (not shared
    /// middleware) since `IntentState` and `crate::api::AppState` are
    /// distinct `axum` states.
    pub auth_token: Option<Arc<str>>,
    /// `Some` when `--ha-peers` is set (phase 12 slice 6) — see
    /// `crate::api::AppState::ha`'s doc, the config side of the same knob.
    pub ha: Option<Arc<crate::ha::HaHandle>>,
}

impl IntentState {
    pub fn new(store: Arc<Store>, role: RoleHandle, auth_token: Option<String>) -> Self {
        let (updates, _rx) = broadcast::channel(UPDATES_CAPACITY);
        IntentState {
            store,
            updates,
            role,
            auth_token: auth_token.map(Arc::from),
            ha: None,
        }
    }

    pub fn with_ha(mut self, ha: Option<Arc<crate::ha::HaHandle>>) -> Self {
        self.ha = ha;
        self
    }

    /// Same bypass [`crate::api::AppState::apply_revision`] provides for
    /// config — the sanctioned way a future intent relay would land an op
    /// received from a parent, regardless of this tier's own role.
    pub fn apply_revision(&self, bytes: RevisionBytes) -> Result<u64, StoreError> {
        let revision = self.store.put(bytes)?;
        let _ = self.updates.send(revision);
        Ok(revision)
    }
}

pub fn router(state: IntentState) -> Router {
    Router::new()
        .route("/intent", axum::routing::post(submit_intent))
        .route("/intent/subscribe", get(subscribe))
        .route_layer(axum::middleware::from_fn_with_state(
            gsp_http::server::BearerAuth::new(state.auth_token.as_deref()),
            gsp_http::server::require_bearer,
        ))
        .with_state(state)
}

#[derive(Serialize)]
struct SubmitResponse {
    revision: u64,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

/// `POST /intent` — body is one JSON [`IntentOp`]. Validated
/// ([`IntentOp::validate`]) and structurally parsed before it ever reaches
/// the store — a malformed or invalid op is rejected here, never broadcast.
async fn submit_intent(State(state): State<IntentState>, body: String) -> Response {
    if state.role.get() == Role::Slave {
        return (
            StatusCode::FORBIDDEN,
            Json(ErrorResponse {
                error: "this controller is a slave tier and does not accept intent writes \
                        directly; submit to the root controller instead"
                    .into(),
            }),
        )
            .into_response();
    }

    let op: IntentOp = match serde_json::from_str(&body) {
        Ok(op) => op,
        Err(e) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(ErrorResponse {
                    error: format!("malformed intent op: {e}"),
                }),
            )
                .into_response()
        }
    };
    if let Err(e) = op.validate() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(ErrorResponse { error: e }),
        )
            .into_response();
    }

    if let Some(ha) = &state.ha {
        return crate::ha::client::propose_write(
            ha,
            crate::ha::WriteRequest::Intent(body.clone().into_bytes()),
            "/intent",
            body,
            None, // intent has no audit-trail actor in this slice — config only
        )
        .await;
    }

    match state.apply_revision(body.into_bytes()) {
        Ok(revision) => {
            tracing::info!(revision, ?op, "accepted a new intent op");
            (StatusCode::OK, Json(SubmitResponse { revision })).into_response()
        }
        Err(e) => store_error_response(e),
    }
}

#[derive(Deserialize)]
struct SubscribeParams {
    since: Option<u64>,
}

/// `GET /intent/subscribe?since=<revision>` — the exact catch-up-then-tail
/// shape `crate::api::subscribe` uses for config, applied to the intent log
/// instead.
async fn subscribe(
    State(state): State<IntentState>,
    Query(params): Query<SubscribeParams>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let (tx, rx) = mpsc::channel(16);
    let updates = state.updates.subscribe();
    tokio::spawn(subscribe_worker(
        state.store,
        updates,
        params.since.unwrap_or(0),
        tx,
    ));

    let events = ReceiverStream::new(rx).map(|(revision, bytes)| {
        let op = String::from_utf8_lossy(&bytes).into_owned();
        let payload = serde_json::json!({ "revision": revision, "op": serde_json::from_str::<serde_json::Value>(&op).unwrap_or(serde_json::Value::Null) });
        Ok(Event::default().data(payload.to_string()))
    });
    Sse::new(events).keep_alive(KeepAlive::default())
}

async fn subscribe_worker(
    store: Arc<Store>,
    mut updates: broadcast::Receiver<u64>,
    since: u64,
    tx: mpsc::Sender<(u64, RevisionBytes)>,
) {
    let mut last_sent = since;

    if !catch_up(&store, &mut last_sent, &tx).await {
        return;
    }

    loop {
        match updates.recv().await {
            Ok(revision) if revision <= last_sent => {}
            Ok(revision) => match store.get(revision) {
                Ok(Some(bytes)) => {
                    if tx.send((revision, bytes)).await.is_err() {
                        return;
                    }
                    last_sent = revision;
                }
                Ok(None) => {
                    tracing::warn!(
                        revision,
                        "update notification for an intent revision store lost"
                    );
                }
                Err(e) => {
                    tracing::error!(error = %e, "store error tailing intent updates");
                    return;
                }
            },
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                tracing::warn!(
                    skipped,
                    "intent subscriber lagged; replaying from the store"
                );
                if !catch_up(&store, &mut last_sent, &tx).await {
                    return;
                }
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

async fn catch_up(
    store: &Store,
    last_sent: &mut u64,
    tx: &mpsc::Sender<(u64, RevisionBytes)>,
) -> bool {
    let revisions = match store.revisions_after(*last_sent) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "store error building the intent catch-up range");
            return false;
        }
    };
    for (revision, bytes) in revisions {
        if tx.send((revision, bytes)).await.is_err() {
            return false;
        }
        *last_sent = revision;
    }
    true
}

fn store_error_response(e: StoreError) -> Response {
    tracing::error!(error = %e, "store error serving the intent API");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse {
            error: e.to_string(),
        }),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn test_state() -> (IntentState, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        (
            IntentState::new(store, RoleHandle::new(Role::Standalone), None),
            dir,
        )
    }

    #[tokio::test]
    async fn a_valid_op_is_accepted() {
        let (state, _dir) = test_state();
        let app = router(state);
        let resp = app
            .oneshot(
                Request::post("/intent")
                    .body(Body::from(
                        r#"{"op":"backend_add","pool":"mc","addr":"127.0.0.1:1"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn an_invalid_op_is_rejected_before_touching_the_store() {
        let (state, _dir) = test_state();
        let app = router(state);
        let resp = app
            .clone()
            .oneshot(
                Request::post("/intent")
                    .body(Body::from(
                        r#"{"op":"backend_add","pool":"mc","addr":"garbage"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn malformed_json_is_rejected() {
        let (state, _dir) = test_state();
        let app = router(state);
        let resp = app
            .oneshot(
                Request::post("/intent")
                    .body(Body::from("not json"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn a_slave_tier_rejects_a_direct_intent_write() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        let state = IntentState::new(store, RoleHandle::new(Role::Slave), None);
        let app = router(state);
        let resp = app
            .oneshot(
                Request::post("/intent")
                    .body(Body::from(
                        r#"{"op":"backend_add","pool":"mc","addr":"127.0.0.1:1"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn subscribe_worker_sends_the_catch_up_range_then_tails() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        let rev1 = store
            .put(br#"{"op":"backend_add","pool":"mc","addr":"127.0.0.1:1"}"#.to_vec())
            .unwrap();

        let (updates_tx, updates_rx) = broadcast::channel(8);
        let (tx, mut rx) = mpsc::channel(8);
        tokio::spawn(subscribe_worker(store.clone(), updates_rx, 0, tx));

        let (got_rev, _) = rx.recv().await.unwrap();
        assert_eq!(got_rev, rev1);

        let rev2 = store
            .put(br#"{"op":"backend_remove","pool":"mc","addr":"127.0.0.1:1"}"#.to_vec())
            .unwrap();
        updates_tx.send(rev2).unwrap();
        let (got_rev2, _) = rx.recv().await.unwrap();
        assert_eq!(got_rev2, rev2);
    }
}
