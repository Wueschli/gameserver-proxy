//! `POST /proxy-peers` / `GET /proxy-peers` / `GET /proxy-peers/{name}` /
//! `GET /proxy-peers/subscribe` — the proxy-peers registry's HTTP surface.
//! Identical shape to `crate::peers::api`, mirrored rather than shared — see
//! the module doc in `crate::proxy_peers` for why.

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

use super::ProxyRegistration;
use crate::addresses::api::claim_error_response;
use crate::addresses::{now_secs, AddressBook, Role};
use crate::peers::{event_payload, tombstone_bytes};
use crate::store::{RevisionBytes, Store, StoreError};

const UPDATES_CAPACITY: usize = 64;

#[derive(Clone)]
pub struct ProxyPeersState {
    store: Arc<Store>,
    /// `name -> latest revision number` (big-endian `u64`) — same sibling-
    /// tree pattern `crate::peers::api::PeersState::current` uses.
    current: sled::Tree,
    updates: broadcast::Sender<u64>,
    auth_token: Option<Arc<str>>,
    /// The shared tunnel-address book every registration claims from.
    book: Arc<AddressBook>,
    /// Serialises POST (claim -> register) against DELETE (check -> remove ->
    /// release) so a re-register can never interleave with a delete and leave
    /// a live registration whose address the book freed. Never held across an
    /// `.await`.
    write_lock: Arc<std::sync::Mutex<()>>,
}

impl ProxyPeersState {
    pub fn new(store: Arc<Store>, auth_token: Option<String>, book: Arc<AddressBook>) -> Self {
        let (updates, _rx) = broadcast::channel(UPDATES_CAPACITY);
        let current = store
            .db()
            .open_tree("current")
            .expect("opening the proxy-peers current tree");
        ProxyPeersState {
            store,
            current,
            updates,
            auth_token: auth_token.map(Arc::from),
            book,
            write_lock: Arc::new(std::sync::Mutex::new(())),
        }
    }

    fn register(&self, reg: &ProxyRegistration) -> Result<u64, StoreError> {
        let bytes = serde_json::to_vec(reg).expect("ProxyRegistration always serializes");
        let revision = self.store.put(bytes)?;
        self.current
            .insert(reg.name.as_bytes(), &revision.to_be_bytes())?;
        self.current.flush()?;
        let _ = self.updates.send(revision);
        Ok(revision)
    }

    fn remove(&self, name: &str) -> Result<u64, StoreError> {
        let revision = self.store.put(tombstone_bytes(name))?;
        self.current.remove(name.as_bytes())?;
        self.current.flush()?;
        let _ = self.updates.send(revision);
        Ok(revision)
    }

    fn current_for(&self, name: &str) -> Result<Option<ProxyRegistration>, StoreError> {
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

    fn all_current(&self) -> Result<Vec<ProxyRegistration>, StoreError> {
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

fn decode_registration(bytes: &[u8]) -> ProxyRegistration {
    serde_json::from_slice(bytes)
        .expect("only ProxyPeersState::register ever writes into this store")
}

pub fn router(state: ProxyPeersState) -> Router {
    Router::new()
        .route("/proxy-peers", axum::routing::post(register).get(list))
        .route("/proxy-peers/subscribe", get(subscribe))
        .route("/proxy-peers/{name}", get(get_one).delete(delete_one))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_bearer,
        ))
        .with_state(state)
}

/// Mirrors `crate::peers::api::require_bearer` exactly, typed against
/// `ProxyPeersState`.
async fn require_bearer(
    State(state): State<ProxyPeersState>,
    req: Request,
    next: Next,
) -> Response {
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

#[derive(Serialize)]
struct SubmitResponse {
    revision: u64,
    tunnel_address: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tunnel_network: Option<String>,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

async fn register(State(state): State<ProxyPeersState>, body: String) -> Response {
    let mut reg: ProxyRegistration = match serde_json::from_str(&body) {
        Ok(reg) => reg,
        Err(e) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(ErrorResponse {
                    error: format!("malformed proxy registration: {e}"),
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

    let guard = state.write_lock.lock().unwrap_or_else(|e| e.into_inner());
    let assignment =
        match state
            .book
            .claim(Role::Proxy, &reg.name, reg.requested_address(), now_secs())
        {
            Ok(a) => a,
            Err(e) => return claim_error_response(&e),
        };
    reg.tunnel_address = Some(assignment.address.to_string());

    let registered = state.register(&reg);
    drop(guard);
    match registered {
        Ok(revision) => {
            tracing::info!(
                revision,
                name = %reg.name,
                address = %assignment.address,
                "registered a proxy peer"
            );
            (
                StatusCode::OK,
                Json(SubmitResponse {
                    revision,
                    tunnel_address: assignment.address.to_string(),
                    tunnel_network: state.book.network().map(|n| n.to_string()),
                }),
            )
                .into_response()
        }
        Err(e) => store_error_response(e),
    }
}

async fn list(State(state): State<ProxyPeersState>) -> Response {
    match state.all_current() {
        Ok(regs) => Json(regs).into_response(),
        Err(e) => store_error_response(e),
    }
}

async fn get_one(State(state): State<ProxyPeersState>, Path(name): Path<String>) -> Response {
    match state.current_for(&name) {
        Ok(Some(reg)) => Json(reg).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("no proxy registered as {name:?}"),
            }),
        )
            .into_response(),
        Err(e) => store_error_response(e),
    }
}

#[derive(Serialize)]
struct DeleteResponse {
    revision: u64,
    released: Option<String>,
}

/// `DELETE /proxy-peers/{name}` — see `crate::peers::api::delete_one`.
async fn delete_one(State(state): State<ProxyPeersState>, Path(name): Path<String>) -> Response {
    let guard = state.write_lock.lock().unwrap_or_else(|e| e.into_inner());
    let has_current = match state.current.contains_key(name.as_bytes()) {
        Ok(b) => b,
        Err(e) => return store_error_response(StoreError::from(e)),
    };
    let has_address = match state.book.get(Role::Proxy, &name) {
        Ok(a) => a.is_some(),
        Err(e) => return claim_error_response(&e),
    };
    if !has_current && !has_address {
        return (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("no proxy registered as {name:?}"),
            }),
        )
            .into_response();
    }
    let revision = match state.remove(&name) {
        Ok(r) => r,
        Err(e) => return store_error_response(e),
    };
    let released = match state.book.release(Role::Proxy, &name) {
        Ok(a) => a,
        Err(e) => return claim_error_response(&e),
    };
    drop(guard);
    tracing::info!(revision, name = %name, address = ?released, "released a proxy peer");
    (
        StatusCode::OK,
        Json(DeleteResponse {
            revision,
            released: released.map(|a| a.to_string()),
        }),
    )
        .into_response()
}

#[derive(Deserialize)]
struct SubscribeParams {
    since: Option<u64>,
}

async fn subscribe(
    State(state): State<ProxyPeersState>,
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
        Ok(Event::default().data(event_payload(revision, &bytes).to_string()))
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
                        "update notification for a proxy-peers revision the store lost"
                    );
                }
                Err(e) => {
                    tracing::error!(error = %e, "store error tailing proxy-peers updates");
                    return;
                }
            },
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                tracing::warn!(
                    skipped,
                    "proxy-peers subscriber lagged; replaying from the store"
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
            tracing::error!(error = %e, "store error building the proxy-peers catch-up range");
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
    tracing::error!(error = %e, "store error serving the proxy-peers API");
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

    use crate::addresses::{AddressBook, Network, Role};

    fn test_state() -> (ProxyPeersState, Arc<AddressBook>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("proxy-peers")).unwrap());
        let book = Arc::new(
            AddressBook::open(
                &dir.path().join("addresses"),
                Some(Network::parse("10.60.0.0/16").unwrap()),
            )
            .unwrap(),
        );
        (ProxyPeersState::new(store, None, book.clone()), book, dir)
    }

    async fn post(app: &Router, body: String) -> (StatusCode, serde_json::Value) {
        let resp = app
            .clone()
            .oneshot(
                Request::post("/proxy-peers")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    fn body_with(name: &str, tunnel_address: Option<&str>) -> String {
        let mut v = serde_json::json!({
            "name": name,
            "pubkey": KEY,
            "endpoint": "203.0.113.9:51820",
        });
        if let Some(a) = tunnel_address {
            v["tunnel_address"] = serde_json::json!(a);
        }
        v.to_string()
    }

    fn reg_body(name: &str, endpoint: &str) -> String {
        serde_json::json!({
            "name": name,
            "pubkey": KEY,
            "endpoint": endpoint,
        })
        .to_string()
    }

    #[tokio::test]
    async fn a_valid_registration_is_accepted() {
        let (state, _book, _dir) = test_state();
        let app = router(state);
        let resp = app
            .oneshot(
                Request::post("/proxy-peers")
                    .body(Body::from(reg_body("edge-1", "203.0.113.9:51820")))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn an_invalid_registration_is_rejected_before_touching_the_store() {
        let (state, _book, _dir) = test_state();
        let app = router(state);
        let resp = app
            .oneshot(
                Request::post("/proxy-peers")
                    .body(Body::from(
                        r#"{"name":"edge-1","pubkey":"garbage","endpoint":"x"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn malformed_json_is_rejected() {
        let (state, _book, _dir) = test_state();
        let app = router(state);
        let resp = app
            .oneshot(
                Request::post("/proxy-peers")
                    .body(Body::from("not json"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn get_one_is_404_before_any_registration() {
        let (state, _book, _dir) = test_state();
        let app = router(state);
        let resp = app
            .oneshot(
                Request::get("/proxy-peers/nope")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_second_registration_replaces_the_current_view_for_that_name() {
        let (state, _book, _dir) = test_state();
        let app = router(state);

        app.clone()
            .oneshot(
                Request::post("/proxy-peers")
                    .body(Body::from(reg_body("edge-1", "203.0.113.9:51820")))
                    .unwrap(),
            )
            .await
            .unwrap();
        app.clone()
            .oneshot(
                Request::post("/proxy-peers")
                    .body(Body::from(reg_body("edge-1", "203.0.113.9:51821")))
                    .unwrap(),
            )
            .await
            .unwrap();

        let resp = app
            .oneshot(
                Request::get("/proxy-peers/edge-1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let reg: ProxyRegistration = serde_json::from_slice(&body).unwrap();
        assert_eq!(reg.endpoint, "203.0.113.9:51821");
    }

    #[tokio::test]
    async fn list_returns_every_currently_registered_proxy() {
        let (state, _book, _dir) = test_state();
        let app = router(state);
        app.clone()
            .oneshot(
                Request::post("/proxy-peers")
                    .body(Body::from(reg_body("a", "203.0.113.1:51820")))
                    .unwrap(),
            )
            .await
            .unwrap();
        app.clone()
            .oneshot(
                Request::post("/proxy-peers")
                    .body(Body::from(reg_body("b", "203.0.113.2:51820")))
                    .unwrap(),
            )
            .await
            .unwrap();

        let resp = app
            .oneshot(Request::get("/proxy-peers").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let regs: Vec<ProxyRegistration> = serde_json::from_slice(&body).unwrap();
        let mut names: Vec<_> = regs.into_iter().map(|r| r.name).collect();
        names.sort();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[tokio::test]
    async fn a_missing_bearer_token_is_rejected_when_one_is_configured() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("proxy-peers")).unwrap());
        let book = Arc::new(
            AddressBook::open(
                &dir.path().join("addresses"),
                Some(Network::parse("10.60.0.0/16").unwrap()),
            )
            .unwrap(),
        );
        let state = ProxyPeersState::new(store, Some("secret".into()), book);
        let app = router(state);
        let resp = app
            .oneshot(Request::get("/proxy-peers").body(Body::empty()).unwrap())
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
                serde_json::to_vec(&ProxyRegistration {
                    name: "edge-1".into(),
                    pubkey: KEY.into(),
                    endpoint: "203.0.113.9:51820".into(),
                    tunnel_address: None,
                    boot_id: None,
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
                serde_json::to_vec(&ProxyRegistration {
                    name: "edge-1".into(),
                    pubkey: KEY.into(),
                    endpoint: "203.0.113.9:51821".into(),
                    tunnel_address: None,
                    boot_id: None,
                })
                .unwrap(),
            )
            .unwrap();
        updates_tx.send(rev2).unwrap();
        let (got_rev2, _) = rx.recv().await.unwrap();
        assert_eq!(got_rev2, rev2);
    }

    #[tokio::test]
    async fn a_registration_without_an_address_is_allocated_one_and_told_the_network() {
        let (state, _book, _dir) = test_state();
        let app = router(state);
        let (status, body) = post(&app, body_with("edge-1", None)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["tunnel_address"], "10.60.0.1");
        assert_eq!(body["tunnel_network"], "10.60.0.0/16");
    }

    #[tokio::test]
    async fn re_registering_returns_the_same_address_and_two_proxies_get_distinct_ones() {
        let (state, _book, _dir) = test_state();
        let app = router(state);
        let (_, first) = post(&app, body_with("a", None)).await;
        let (_, again) = post(&app, body_with("a", None)).await;
        let (_, other) = post(&app, body_with("b", None)).await;
        assert_eq!(first["tunnel_address"], again["tunnel_address"]);
        assert_ne!(first["tunnel_address"], other["tunnel_address"]);
    }

    #[tokio::test]
    async fn a_pinned_address_held_by_an_origin_is_a_409_naming_the_holder() {
        let (state, book, _dir) = test_state();
        book.claim(Role::Origin, "home", Some("10.60.0.9".parse().unwrap()), 1)
            .unwrap();
        let app = router(state);
        let (status, body) = post(&app, body_with("edge-1", Some("10.60.0.9"))).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(body["error"].as_str().unwrap().contains("origin \"home\""));
    }

    #[tokio::test]
    async fn a_pin_outside_the_network_is_a_422() {
        let (state, _book, _dir) = test_state();
        let app = router(state);
        let (status, _) = post(&app, body_with("edge-1", Some("192.168.1.5"))).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn the_stored_registration_carries_the_allocated_address() {
        let (state, _book, _dir) = test_state();
        let app = router(state);
        post(&app, body_with("edge-1", None)).await;
        let resp = app
            .oneshot(
                Request::get("/proxy-peers/edge-1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let reg: ProxyRegistration = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(reg.tunnel_address.as_deref(), Some("10.60.0.1"));
    }

    #[tokio::test]
    async fn the_boot_id_is_stored_as_submitted_and_a_bad_one_is_a_422() {
        let (state, _book, _dir) = test_state();
        let app = router(state);
        let mut v: serde_json::Value = serde_json::from_str(&body_with("edge-1", None)).unwrap();
        v["boot_id"] = serde_json::json!("0123456789abcdef");
        assert_eq!(post(&app, v.to_string()).await.0, StatusCode::OK);
        let resp = app
            .clone()
            .oneshot(
                Request::get("/proxy-peers/edge-1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let reg: ProxyRegistration = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(reg.boot_id.as_deref(), Some("0123456789abcdef"));

        v["boot_id"] = serde_json::json!("not valid!");
        assert_eq!(
            post(&app, v.to_string()).await.0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }

    async fn delete(app: &Router, name: &str) -> (StatusCode, serde_json::Value) {
        let resp = app
            .clone()
            .oneshot(
                Request::delete(format!("/proxy-peers/{name}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    #[tokio::test]
    async fn delete_frees_the_address_and_removes_the_current_registration() {
        let (state, _book, _dir) = test_state();
        let app = router(state);
        let (_, a) = post(&app, body_with("a", None)).await;
        assert_eq!(a["tunnel_address"], "10.60.0.1");

        let (status, body) = delete(&app, "a").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["released"], "10.60.0.1");

        let resp = app
            .clone()
            .oneshot(Request::get("/proxy-peers/a").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let (_, b) = post(&app, body_with("b", None)).await;
        assert_eq!(b["tunnel_address"], "10.60.0.1");
    }

    #[tokio::test]
    async fn delete_of_an_unknown_name_is_404() {
        let (state, _book, _dir) = test_state();
        let app = router(state);
        let (status, _) = delete(&app, "nobody").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    const ROUNDS: usize = 200;

    /// A POST racing a DELETE for the same name must never leave a live
    /// registration whose address the book has freed (or the reverse).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn post_racing_delete_never_splits_registry_and_book() {
        let (state, book, _dir) = test_state();
        let app = router(state.clone());
        for round in 0..ROUNDS {
            post(&app, body_with("race", None)).await;
            let _ = tokio::join!(
                tokio::spawn({
                    let app = app.clone();
                    async move { post(&app, body_with("race", None)).await }
                }),
                tokio::spawn({
                    let app = app.clone();
                    async move { delete(&app, "race").await }
                }),
            );
            let current = state.current_for("race").unwrap();
            let held = book.get(Role::Proxy, "race").unwrap();
            match (&current, &held) {
                (None, None) => {}
                (Some(c), Some(h)) => assert_eq!(
                    c.tunnel_address.as_deref(),
                    Some(h.address.to_string().as_str()),
                    "round {round}: registration and book disagree on the address"
                ),
                _ => panic!(
                    "round {round}: registration present={} but book present={}",
                    current.is_some(),
                    held.is_some()
                ),
            }
            // Reset for the next round.
            let _ = delete(&app, "race").await;
        }
    }

    #[tokio::test]
    async fn delete_logs_a_tombstone_a_catch_up_subscriber_receives() {
        let (state, _book, _dir) = test_state();
        let app = router(state.clone());
        post(&app, body_with("edge-1", None)).await;
        delete(&app, "edge-1").await;

        let (tx, mut rx) = mpsc::channel(8);
        let updates = state.updates.subscribe();
        tokio::spawn(subscribe_worker(state.store.clone(), updates, 0, tx));
        let (r1, b1) = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("timed out waiting for event 1")
            .expect("channel closed before event 1");
        let (r2, b2) = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("timed out waiting for event 2")
            .expect("channel closed before event 2");
        assert!(r2 > r1);
        assert_eq!(
            crate::peers::event_payload(r1, &b1)["registration"]["name"],
            "edge-1"
        );
        assert_eq!(
            crate::peers::event_payload(r2, &b2)["removed"]["name"],
            "edge-1"
        );
    }
}
