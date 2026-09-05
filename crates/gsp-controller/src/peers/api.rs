//! `POST /peers` / `GET /peers` / `GET /peers/{name}` / `GET
//! /peers/subscribe` — the backend-peers registry's HTTP surface. See the
//! module doc in `crate::peers` for the shape ("per-origin latest-write-wins
//! state" on top of `Store`'s append-only log).

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Path, Query, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::{Stream, StreamExt};

use super::PeerRegistration;
use crate::store::{RevisionBytes, Store, StoreError};

const UPDATES_CAPACITY: usize = 64;

#[derive(Clone)]
pub struct PeersState {
    store: Arc<Store>,
    /// `name -> latest revision number` (big-endian `u64`), a sibling tree
    /// in the same `sled` database `store` opened — see `Store::db`'s doc
    /// and `crate::api::AppState::stage`'s identical pattern. Lets
    /// `GET /peers`/`GET /peers/{name}` answer "what's current" without
    /// scanning the whole log.
    current: sled::Tree,
    updates: broadcast::Sender<u64>,
    /// Bearer token every `/peers*` request must present, or `None` to
    /// leave the API open — same posture as `crate::api::AppState`'s and
    /// `crate::intent::api::IntentState`'s own `auth_token`.
    auth_token: Option<Arc<str>>,
}

impl PeersState {
    pub fn new(store: Arc<Store>, auth_token: Option<String>) -> Self {
        let (updates, _rx) = broadcast::channel(UPDATES_CAPACITY);
        let current = store
            .db()
            .open_tree("current")
            .expect("opening the peers current tree");
        PeersState {
            store,
            current,
            updates,
            auth_token: auth_token.map(Arc::from),
        }
    }

    /// Persists `reg` as a new revision and updates `current[reg.name]` to
    /// point at it in the same `sled` transaction — a crash can never leave
    /// `current` pointing at a revision the log doesn't have, or vice versa.
    fn register(&self, reg: &PeerRegistration) -> Result<u64, StoreError> {
        let bytes = serde_json::to_vec(reg).expect("PeerRegistration always serializes");
        let revision = self.store.put(bytes)?;
        self.current
            .insert(reg.name.as_bytes(), &revision.to_be_bytes())?;
        self.current.flush()?;
        let _ = self.updates.send(revision);
        Ok(revision)
    }

    /// The current registration for `name`, if it has ever registered.
    fn current_for(&self, name: &str) -> Result<Option<PeerRegistration>, StoreError> {
        let Some(rev_bytes) = self.current.get(name.as_bytes())? else {
            return Ok(None);
        };
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&rev_bytes);
        let revision = u64::from_be_bytes(buf);
        Ok(self
            .store
            .get(revision)?
            .map(|bytes| decode_registration(&bytes)))
    }

    /// Every origin's current registration, in no particular order.
    fn all_current(&self) -> Result<Vec<PeerRegistration>, StoreError> {
        let mut out = Vec::new();
        for item in self.current.iter() {
            let (_, rev_bytes) = item?;
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&rev_bytes);
            let revision = u64::from_be_bytes(buf);
            if let Some(bytes) = self.store.get(revision)? {
                out.push(decode_registration(&bytes));
            }
        }
        Ok(out)
    }
}

fn decode_registration(bytes: &[u8]) -> PeerRegistration {
    serde_json::from_slice(bytes).expect("only PeersState::register ever writes into this store")
}

pub fn router(state: PeersState) -> Router {
    Router::new()
        .route("/peers", axum::routing::post(register).get(list))
        .route("/peers/subscribe", get(subscribe))
        .route("/peers/{name}", get(get_one))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_bearer,
        ))
        .with_state(state)
}

/// Mirrors `crate::intent::api::require_bearer` exactly, typed against
/// `PeersState` — a distinct `axum` state needs its own instance.
async fn require_bearer(State(state): State<PeersState>, req: Request, next: Next) -> Response {
    let Some(expected) = state.auth_token.as_deref() else {
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

#[derive(Serialize)]
struct SubmitResponse {
    revision: u64,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

/// `POST /peers` — body is one JSON [`PeerRegistration`]. Validated before it
/// ever reaches the store, same posture as `crate::intent::api::submit_intent`.
async fn register(State(state): State<PeersState>, body: String) -> Response {
    let reg: PeerRegistration = match serde_json::from_str(&body) {
        Ok(reg) => reg,
        Err(e) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(ErrorResponse {
                    error: format!("malformed peer registration: {e}"),
                }),
            )
                .into_response()
        }
    };
    if let Err(e) = reg.validate() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(ErrorResponse { error: e }),
        )
            .into_response();
    }

    match state.register(&reg) {
        Ok(revision) => {
            tracing::info!(revision, name = %reg.name, "registered a backend peer");
            (StatusCode::OK, Json(SubmitResponse { revision })).into_response()
        }
        Err(e) => store_error_response(e),
    }
}

/// `GET /peers` — every origin's current registration.
async fn list(State(state): State<PeersState>) -> Response {
    match state.all_current() {
        Ok(regs) => Json(regs).into_response(),
        Err(e) => store_error_response(e),
    }
}

/// `GET /peers/{name}` — one origin's current registration, `404` if it has
/// never registered.
async fn get_one(State(state): State<PeersState>, Path(name): Path<String>) -> Response {
    match state.current_for(&name) {
        Ok(Some(reg)) => Json(reg).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("no peer registered as {name:?}"),
            }),
        )
            .into_response(),
        Err(e) => store_error_response(e),
    }
}

#[derive(Deserialize)]
struct SubscribeParams {
    since: Option<u64>,
}

/// `GET /peers/subscribe?since=<revision>` — the exact catch-up-then-tail
/// shape `crate::intent::api::subscribe` uses, applied to the peers log.
/// Every event is a full [`PeerRegistration`]; a subscriber (the phase 14
/// slice 4 `gsp` reconcile task) keeps its own latest-by-name view, exactly
/// like this module's own `current` tree.
async fn subscribe(
    State(state): State<PeersState>,
    Query(params): Query<SubscribeParams>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let (tx, rx) = mpsc::channel(16);
    let updates = state.updates.subscribe();
    tokio::spawn(subscribe_worker(
        state.store.clone(),
        updates,
        params.since.unwrap_or(0),
        tx,
    ));

    let events = ReceiverStream::new(rx).map(|(revision, bytes)| {
        let reg = String::from_utf8_lossy(&bytes).into_owned();
        let payload = serde_json::json!({ "revision": revision, "registration": serde_json::from_str::<serde_json::Value>(&reg).unwrap_or(serde_json::Value::Null) });
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
                        "update notification for a peers revision the store lost"
                    );
                }
                Err(e) => {
                    tracing::error!(error = %e, "store error tailing peers updates");
                    return;
                }
            },
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                tracing::warn!(skipped, "peers subscriber lagged; replaying from the store");
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
            tracing::error!(error = %e, "store error building the peers catch-up range");
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
    tracing::error!(error = %e, "store error serving the peers API");
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

    const KEY: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

    fn test_state() -> (PeersState, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        (PeersState::new(store, None), dir)
    }

    fn reg_body(name: &str, backends: &[&str]) -> String {
        serde_json::json!({
            "name": name,
            "pubkey": KEY,
            "endpoint": "203.0.113.7:51820",
            "backends": backends,
        })
        .to_string()
    }

    #[tokio::test]
    async fn a_valid_registration_is_accepted() {
        let (state, _dir) = test_state();
        let app = router(state);
        let resp = app
            .oneshot(
                Request::post("/peers")
                    .body(Body::from(reg_body("home", &["10.60.0.2:25565"])))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn an_invalid_registration_is_rejected_before_touching_the_store() {
        let (state, _dir) = test_state();
        let app = router(state);
        let resp = app
            .oneshot(
                Request::post("/peers")
                    .body(Body::from(r#"{"name":"home","pubkey":"garbage"}"#))
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
                Request::post("/peers")
                    .body(Body::from("not json"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn get_one_is_404_before_any_registration() {
        let (state, _dir) = test_state();
        let app = router(state);
        let resp = app
            .oneshot(Request::get("/peers/nope").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_second_registration_replaces_the_current_view_for_that_name() {
        let (state, _dir) = test_state();
        let app = router(state);

        app.clone()
            .oneshot(
                Request::post("/peers")
                    .body(Body::from(reg_body("home", &["10.60.0.2:1"])))
                    .unwrap(),
            )
            .await
            .unwrap();
        app.clone()
            .oneshot(
                Request::post("/peers")
                    .body(Body::from(reg_body("home", &["10.60.0.2:2"])))
                    .unwrap(),
            )
            .await
            .unwrap();

        let resp = app
            .oneshot(Request::get("/peers/home").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let reg: PeerRegistration = serde_json::from_slice(&body).unwrap();
        assert_eq!(reg.backends, vec!["10.60.0.2:2"]);
    }

    #[tokio::test]
    async fn list_returns_every_currently_registered_origin() {
        let (state, _dir) = test_state();
        let app = router(state);
        app.clone()
            .oneshot(
                Request::post("/peers")
                    .body(Body::from(reg_body("a", &[])))
                    .unwrap(),
            )
            .await
            .unwrap();
        app.clone()
            .oneshot(
                Request::post("/peers")
                    .body(Body::from(reg_body("b", &[])))
                    .unwrap(),
            )
            .await
            .unwrap();

        let resp = app
            .oneshot(Request::get("/peers").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let regs: Vec<PeerRegistration> = serde_json::from_slice(&body).unwrap();
        let mut names: Vec<_> = regs.into_iter().map(|r| r.name).collect();
        names.sort();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[tokio::test]
    async fn a_missing_bearer_token_is_rejected_when_one_is_configured() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        let state = PeersState::new(store, Some("secret".into()));
        let app = router(state);
        let resp = app
            .oneshot(Request::get("/peers").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn subscribe_worker_sends_the_catch_up_range_then_tails() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        let rev1 = store
            .put(
                serde_json::to_vec(&PeerRegistration {
                    name: "home".into(),
                    pubkey: KEY.into(),
                    endpoint: None,
                    backends: vec![],
                })
                .unwrap(),
            )
            .unwrap();

        let (updates_tx, updates_rx) = broadcast::channel(8);
        let (tx, mut rx) = mpsc::channel(8);
        tokio::spawn(subscribe_worker(store.clone(), updates_rx, 0, tx));

        let (got_rev, _) = rx.recv().await.unwrap();
        assert_eq!(got_rev, rev1);

        let rev2 = store
            .put(
                serde_json::to_vec(&PeerRegistration {
                    name: "home".into(),
                    pubkey: KEY.into(),
                    endpoint: None,
                    backends: vec!["10.60.0.2:1".into()],
                })
                .unwrap(),
            )
            .unwrap();
        updates_tx.send(rev2).unwrap();
        let (got_rev2, _) = rx.recv().await.unwrap();
        assert_eq!(got_rev2, rev2);
    }
}
