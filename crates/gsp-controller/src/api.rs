//! `POST /config` / `GET /config` — the config submit API (slice 2). Every
//! submission runs the **same `gsp_config::parse_str`** (parse + `validate()`)
//! a proxy runs on a file reload before it ever reaches [`Store::put`]; a
//! rejected submission never touches the store, so the previous revision
//! stays current — the same "bad reload keeps the old snapshot" rule the
//! proxy already has, one hop earlier.
//!
//! `GET /config` returns the current revision's raw text. Revision history /
//! diff / rollback (slice 5) build on top of the same [`Store`].
//!
//! `GET /config/subscribe?since=<revision>` (slice 3) is the pull side of
//! Tier 1 (`docs/10` principle 5): a subscriber gets the catch-up range
//! (every revision after `since`, from [`Store::revisions_after`]) then a
//! live tail of whatever [`submit_config`] accepts next, as
//! [server-sent events](https://developer.mozilla.org/en-US/docs/Web/API/Server-sent_events).
//! A lagging subscriber (slow reader vs. a burst of submissions) just
//! re-runs the catch-up query from wherever it left off — `Store` never
//! forgets a revision, so there is no delivery state to track here.
//!
//! Slice 5 adds `GET /config/revisions` (history), `GET
//! /config/revisions/{revision}` (one past revision's raw text), `GET
//! /config/revisions/{revision}/diff[?against=<revision>]` (a line diff
//! against `current`, or another revision), and `POST
//! /config/rollback/{revision}` — which **never rewrites history**, it
//! re-submits that revision's bytes as a brand new one through the same
//! validate-then-[`Store::put`] path `submit_config` uses, so a subscriber
//! sees rollback as just another ordinary revision, no special-casing
//! needed anywhere else. The whole surface is gated by
//! [`crate::auth::require_bearer`] when `AppState::auth_token` is set.

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::middleware;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::{Stream, StreamExt};

use crate::role::{Role, RoleHandle};
use crate::store::{RevisionBytes, Store, StoreError};

/// Capacity of the update-notification broadcast: how many accepted
/// submissions can land between two ticks of a subscriber's tail loop before
/// it has to fall back to re-querying the store. Generous for a control
/// plane (submissions are rare, human-paced events).
const UPDATES_CAPACITY: usize = 64;

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Store>,
    /// Notifies every live subscriber of a newly accepted revision number;
    /// the subscriber re-fetches the bytes from `store` itself (the channel
    /// only ever carries a `u64`, never the config text).
    pub updates: broadcast::Sender<u64>,
    /// Bearer token every `/config*` request must present, or `None` to
    /// leave the API open (`crate::auth`).
    pub auth_token: Option<Arc<str>>,
    /// `standalone` (default) accepts writes directly; `slave` (phase 12
    /// slice 1) never does — every revision it holds arrived via
    /// [`crate::parent_client`], which writes the store directly and is the
    /// only caller allowed to bypass this gate. See `crate::role`.
    pub role: RoleHandle,
    /// `Some` when `--ha-peers` is set (phase 12 slice 6): `submit()`
    /// proposes via Raft instead of writing `store` directly. `None` (the
    /// default, `replicas: 1`) is today's behaviour, byte-for-byte —
    /// see `crate::ha`'s module doc for the scope cut (HA and `slave` are
    /// mutually exclusive in this slice).
    pub ha: Option<Arc<crate::ha::HaHandle>>,
}

impl AppState {
    pub fn new(store: Arc<Store>, auth_token: Option<String>, role: RoleHandle) -> Self {
        let (updates, _rx) = broadcast::channel(UPDATES_CAPACITY);
        AppState {
            store,
            updates,
            auth_token: auth_token.map(Arc::from),
            role,
            ha: None,
        }
    }

    pub fn with_ha(mut self, ha: Option<Arc<crate::ha::HaHandle>>) -> Self {
        self.ha = ha;
        self
    }

    /// Persists `bytes` as a new revision and notifies subscribers — the one
    /// path both a local `submit()` and [`crate::parent_client`]'s relay use
    /// to actually land a revision, after each has done its own role check
    /// (a local submit must be rejected first; a parent relay is always
    /// allowed regardless of role).
    pub fn apply_revision(&self, bytes: RevisionBytes) -> Result<u64, StoreError> {
        let revision = self.store.put(bytes)?;
        let _ = self.updates.send(revision);
        Ok(revision)
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/config", post(submit_config).get(get_current_config))
        .route("/config/subscribe", get(subscribe))
        .route("/config/revisions", get(list_revisions))
        .route("/config/revisions/{revision}", get(get_revision))
        .route("/config/revisions/{revision}/diff", get(diff_revision))
        .route("/config/rollback/{revision}", post(rollback))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            crate::auth::require_bearer,
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

/// `POST /config` — body is a raw YAML config document, the same shape a
/// proxy's `--config` file has. Validates, then persists on success.
async fn submit_config(State(state): State<AppState>, body: String) -> Response {
    submit(&state, body).await
}

/// Validates `text` and persists it as a new revision on success — shared by
/// [`submit_config`] and [`rollback`] (a rollback is just a re-submission of
/// an old revision's bytes, never a rewrite of history). Rejects outright on
/// a `slave` tier (`docs/10` "never accepts a write directly") before even
/// parsing — a slave's only source of new revisions is
/// [`crate::parent_client`], which never calls this function.
async fn submit(state: &AppState, text: String) -> Response {
    if state.role.get() == Role::Slave {
        return slave_rejects_write();
    }

    if let Err(e) = gsp_config::parse_str(&text) {
        tracing::warn!(error = %e, "rejected an invalid config submission");
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
            .into_response();
    }

    if let Some(ha) = &state.ha {
        return crate::ha::client::propose_write(
            ha,
            crate::ha::WriteRequest::Config(text.clone().into_bytes()),
            "/config",
            text,
        )
        .await;
    }

    match state.apply_revision(text.into_bytes()) {
        Ok(revision) => {
            tracing::info!(revision, "accepted a new config revision");
            (StatusCode::OK, Json(SubmitResponse { revision })).into_response()
        }
        Err(e) => store_error_response(e),
    }
}

fn slave_rejects_write() -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(ErrorResponse {
            error: "this controller is a slave tier and does not accept writes directly; \
                    submit to the root controller instead"
                .into(),
        }),
    )
        .into_response()
}

/// `GET /config` — the current revision's raw text, or 404 before the first
/// submission has ever landed.
async fn get_current_config(State(state): State<AppState>) -> Response {
    match state.store.current() {
        Ok(Some((revision, bytes))) => {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            (
                StatusCode::OK,
                [("X-Config-Revision", revision.to_string())],
                text,
            )
                .into_response()
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "no config has been submitted yet".into(),
            }),
        )
            .into_response(),
        Err(e) => store_error_response(e),
    }
}

#[derive(Serialize)]
struct RevisionSummary {
    revision: u64,
    size_bytes: usize,
    current: bool,
}

/// `GET /config/revisions` — every revision, oldest first, with its size and
/// whether it's the current one. Reuses `revisions_after(0)` rather than
/// adding a store method that keeps only lengths — revisions are YAML config
/// text (KB-sized), and this is a control-plane, human-paced call, not
/// something worth a dedicated storage path.
async fn list_revisions(State(state): State<AppState>) -> Response {
    let current = match state.store.current_revision() {
        Ok(c) => c,
        Err(e) => return store_error_response(e),
    };
    match state.store.revisions_after(0) {
        Ok(revisions) => {
            let summaries: Vec<RevisionSummary> = revisions
                .into_iter()
                .map(|(revision, bytes)| RevisionSummary {
                    revision,
                    size_bytes: bytes.len(),
                    current: Some(revision) == current,
                })
                .collect();
            (StatusCode::OK, Json(summaries)).into_response()
        }
        Err(e) => store_error_response(e),
    }
}

/// `GET /config/revisions/{revision}` — one past revision's raw text (the
/// same shape `GET /config` returns for the current one), `404` if it never
/// existed.
async fn get_revision(State(state): State<AppState>, Path(revision): Path<u64>) -> Response {
    match state.store.get(revision) {
        Ok(Some(bytes)) => {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            (StatusCode::OK, text).into_response()
        }
        Ok(None) => revision_not_found(revision),
        Err(e) => store_error_response(e),
    }
}

#[derive(Deserialize)]
struct DiffParams {
    /// Defaults to the current revision — "what changed between this old
    /// revision and now" is the common question; diffing two arbitrary past
    /// revisions is still possible by passing this explicitly.
    against: Option<u64>,
}

/// `GET /config/revisions/{revision}/diff[?against=<revision>]` — a
/// line-level diff (`similar::TextDiff`) between `revision` and `against`
/// (default: current), rendered as plain text with `+`/`-`/` ` line prefixes.
async fn diff_revision(
    State(state): State<AppState>,
    Path(revision): Path<u64>,
    Query(params): Query<DiffParams>,
) -> Response {
    let old = match state.store.get(revision) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return revision_not_found(revision),
        Err(e) => return store_error_response(e),
    };

    let (against, new) = match params.against {
        Some(rev) => match state.store.get(rev) {
            Ok(Some(bytes)) => (rev, bytes),
            Ok(None) => return revision_not_found(rev),
            Err(e) => return store_error_response(e),
        },
        None => match state.store.current() {
            Ok(Some((rev, bytes))) => (rev, bytes),
            Ok(None) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(ErrorResponse {
                        error: "no current config to diff against".into(),
                    }),
                )
                    .into_response()
            }
            Err(e) => return store_error_response(e),
        },
    };

    let old_text = String::from_utf8_lossy(&old).into_owned();
    let new_text = String::from_utf8_lossy(&new).into_owned();
    let diff = similar::TextDiff::from_lines(&old_text, &new_text);
    let mut out = format!("--- revision {revision}\n+++ revision {against}\n");
    for change in diff.iter_all_changes() {
        let sign = match change.tag() {
            similar::ChangeTag::Delete => '-',
            similar::ChangeTag::Insert => '+',
            similar::ChangeTag::Equal => ' ',
        };
        out.push(sign);
        out.push_str(change.as_str().unwrap_or(""));
    }
    (StatusCode::OK, out).into_response()
}

/// `POST /config/rollback/{revision}` — re-submits an old revision's exact
/// bytes as a brand-new one (see the module doc: this never rewrites
/// history). Same response shape as `POST /config`.
async fn rollback(State(state): State<AppState>, Path(revision): Path<u64>) -> Response {
    match state.store.get(revision) {
        Ok(Some(bytes)) => {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            let resp = submit(&state, text).await;
            tracing::info!(from_revision = revision, "rolled back");
            resp
        }
        Ok(None) => revision_not_found(revision),
        Err(e) => store_error_response(e),
    }
}

fn revision_not_found(revision: u64) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(ErrorResponse {
            error: format!("no revision {revision}"),
        }),
    )
        .into_response()
}

#[derive(Deserialize)]
struct SubscribeParams {
    since: Option<u64>,
}

/// `GET /config/subscribe?since=<revision>` — see the module doc. Spawns
/// [`subscribe_worker`] to do the actual catch-up + tail work and turns its
/// output into SSE `Event`s; the split keeps the worker's logic (the part
/// worth testing) free of any HTTP/SSE framing.
async fn subscribe(
    State(state): State<AppState>,
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
        let config = String::from_utf8_lossy(&bytes).into_owned();
        let payload = serde_json::json!({ "revision": revision, "config": config });
        Ok(Event::default().data(payload.to_string()))
    });
    Sse::new(events).keep_alive(KeepAlive::default())
}

/// Sends the catch-up range for `since`, then tails `updates` for whatever
/// [`submit_config`] accepts next, forever (until the receiving end of `tx`
/// drops — the client disconnected). A [`broadcast::error::RecvError::Lagged`]
/// (the worker fell behind a burst of submissions) just re-runs the catch-up
/// query from wherever it last got to — every revision lives in `store`
/// forever, so nothing is lost, only replayed.
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
            Ok(revision) if revision <= last_sent => {} // already sent by catch-up
            Ok(revision) => match store.get(revision) {
                Ok(Some(bytes)) => {
                    if tx.send((revision, bytes)).await.is_err() {
                        return;
                    }
                    last_sent = revision;
                }
                Ok(None) => {
                    tracing::warn!(revision, "update notification for a revision store lost");
                }
                Err(e) => {
                    tracing::error!(error = %e, "store error tailing config updates");
                    return;
                }
            },
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                tracing::warn!(skipped, "subscriber lagged; replaying from the store");
                if !catch_up(&store, &mut last_sent, &tx).await {
                    return;
                }
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// Sends every revision after `*last_sent`, advancing it as it goes.
/// Returns `false` if the receiver dropped (stop the worker).
async fn catch_up(
    store: &Store,
    last_sent: &mut u64,
    tx: &mpsc::Sender<(u64, RevisionBytes)>,
) -> bool {
    let revisions = match store.revisions_after(*last_sent) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "store error building the catch-up range");
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
    tracing::error!(error = %e, "store error serving the config API");
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

    fn test_state() -> (AppState, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        (
            AppState::new(store, None, RoleHandle::new(Role::Standalone)),
            dir,
        )
    }

    const VALID_CONFIG: &str = r#"
pools:
  - name: mc
    targets: ["127.0.0.1:25566"]
listeners:
  - name: main
    bind: "0.0.0.0:25565"
    protocol: tcp
    pool: mc
"#;

    #[tokio::test]
    async fn get_config_before_any_submission_is_404() {
        let (state, _dir) = test_state();
        let app = router(state);
        let resp = app
            .oneshot(Request::get("/config").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_valid_submission_is_accepted_and_becomes_current() {
        let (state, _dir) = test_state();
        let app = router(state);

        let resp = app
            .clone()
            .oneshot(
                Request::post("/config")
                    .body(Body::from(VALID_CONFIG))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = app
            .oneshot(Request::get("/config").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers().get("X-Config-Revision").unwrap(), "1");
    }

    #[tokio::test]
    async fn an_invalid_submission_is_rejected_and_leaves_current_untouched() {
        let (state, _dir) = test_state();
        let app = router(state);

        app.clone()
            .oneshot(
                Request::post("/config")
                    .body(Body::from(VALID_CONFIG))
                    .unwrap(),
            )
            .await
            .unwrap();

        let resp = app
            .clone()
            .oneshot(
                Request::post("/config")
                    .body(Body::from("not: [valid, yaml: at all"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);

        let resp = app
            .oneshot(Request::get("/config").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            resp.headers().get("X-Config-Revision").unwrap(),
            "1",
            "the rejected submission must not have bumped the current revision"
        );
    }

    #[tokio::test]
    async fn subscribe_worker_sends_the_catch_up_range_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        let rev1 = store.put(b"one".to_vec()).unwrap();
        let rev2 = store.put(b"two".to_vec()).unwrap();

        let (_updates_tx, updates_rx) = broadcast::channel(8);
        let (tx, mut rx) = mpsc::channel(8);
        tokio::spawn(subscribe_worker(store, updates_rx, 0, tx));

        assert_eq!(rx.recv().await.unwrap(), (rev1, b"one".to_vec()));
        assert_eq!(rx.recv().await.unwrap(), (rev2, b"two".to_vec()));
    }

    #[tokio::test]
    async fn subscribe_worker_tails_a_revision_accepted_after_it_started() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        let rev1 = store.put(b"one".to_vec()).unwrap();

        let (updates_tx, updates_rx) = broadcast::channel(8);
        let (tx, mut rx) = mpsc::channel(8);
        tokio::spawn(subscribe_worker(store.clone(), updates_rx, 0, tx));

        assert_eq!(rx.recv().await.unwrap(), (rev1, b"one".to_vec()));

        let rev2 = store.put(b"two".to_vec()).unwrap();
        updates_tx.send(rev2).unwrap();
        assert_eq!(rx.recv().await.unwrap(), (rev2, b"two".to_vec()));
    }

    #[tokio::test]
    async fn subscribe_worker_since_a_revision_skips_everything_up_to_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        let rev1 = store.put(b"one".to_vec()).unwrap();
        let rev2 = store.put(b"two".to_vec()).unwrap();

        let (_updates_tx, updates_rx) = broadcast::channel(8);
        let (tx, mut rx) = mpsc::channel(8);
        tokio::spawn(subscribe_worker(store, updates_rx, rev1, tx));

        assert_eq!(rx.recv().await.unwrap(), (rev2, b"two".to_vec()));
    }

    #[tokio::test]
    async fn a_lagged_subscriber_replays_from_the_store_instead_of_losing_revisions() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        let rev1 = store.put(b"one".to_vec()).unwrap();

        let (updates_tx, updates_rx) = broadcast::channel(1);
        let (tx, mut rx) = mpsc::channel(8);
        tokio::spawn(subscribe_worker(store.clone(), updates_rx, 0, tx));

        // Initial catch-up delivers rev1.
        assert_eq!(rx.recv().await.unwrap(), (rev1, b"one".to_vec()));

        // Two more revisions land back-to-back; a capacity-1 broadcast
        // channel can only hold the last notification, so whether or not
        // the worker's next `recv()` actually observes `Lagged` (it may win
        // the race and see `rev2` directly) is a scheduling detail — either
        // way it must end up delivering *both* rev2 and rev3, never skip
        // straight to rev3.
        let rev2 = store.put(b"two".to_vec()).unwrap();
        let rev3 = store.put(b"three".to_vec()).unwrap();
        updates_tx.send(rev2).unwrap();
        updates_tx.send(rev3).unwrap();

        assert_eq!(rx.recv().await.unwrap(), (rev2, b"two".to_vec()));
        assert_eq!(rx.recv().await.unwrap(), (rev3, b"three".to_vec()));
    }

    const OTHER_VALID_CONFIG: &str = r#"
pools:
  - name: mc
    targets: ["127.0.0.1:9999"]
listeners:
  - name: main
    bind: "0.0.0.0:25565"
    protocol: tcp
    pool: mc
"#;

    async fn submit(app: &Router, body: &'static str) -> u64 {
        let resp = app
            .clone()
            .oneshot(Request::post("/config").body(Body::from(body)).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        json["revision"].as_u64().unwrap()
    }

    #[tokio::test]
    async fn list_revisions_reports_size_and_marks_current() {
        let (state, _dir) = test_state();
        let app = router(state);
        let rev1 = submit(&app, VALID_CONFIG).await;
        let rev2 = submit(&app, OTHER_VALID_CONFIG).await;

        let resp = app
            .oneshot(
                Request::get("/config/revisions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let list: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0]["revision"], rev1);
        assert_eq!(list[0]["current"], false);
        assert_eq!(list[1]["revision"], rev2);
        assert_eq!(list[1]["current"], true);
        assert_eq!(list[0]["size_bytes"], VALID_CONFIG.len());
    }

    #[tokio::test]
    async fn get_revision_returns_a_past_revisions_text_and_404s_a_missing_one() {
        let (state, _dir) = test_state();
        let app = router(state);
        let rev1 = submit(&app, VALID_CONFIG).await;

        let resp = app
            .clone()
            .oneshot(
                Request::get(format!("/config/revisions/{rev1}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(bytes, VALID_CONFIG.as_bytes());

        let resp = app
            .oneshot(
                Request::get("/config/revisions/9999")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn diff_against_current_shows_the_changed_line() {
        let (state, _dir) = test_state();
        let app = router(state);
        let rev1 = submit(&app, VALID_CONFIG).await;
        submit(&app, OTHER_VALID_CONFIG).await;

        let resp = app
            .oneshot(
                Request::get(format!("/config/revisions/{rev1}/diff"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(text.contains("-    targets: [\"127.0.0.1:25566\"]"));
        assert!(text.contains("+    targets: [\"127.0.0.1:9999\"]"));
    }

    #[tokio::test]
    async fn rollback_resubmits_the_old_bytes_as_a_new_revision() {
        let (state, _dir) = test_state();
        let app = router(state);
        let rev1 = submit(&app, VALID_CONFIG).await;
        submit(&app, OTHER_VALID_CONFIG).await;

        let resp = app
            .clone()
            .oneshot(
                Request::post(format!("/config/rollback/{rev1}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let rollback_rev = json["revision"].as_u64().unwrap();
        assert_eq!(
            rollback_rev, 3,
            "a rollback is a new revision, never a rewrite of history"
        );

        let resp = app
            .oneshot(Request::get("/config").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(bytes, VALID_CONFIG.as_bytes());
    }

    #[tokio::test]
    async fn rollback_to_a_missing_revision_is_404() {
        let (state, _dir) = test_state();
        let app = router(state);
        let resp = app
            .oneshot(
                Request::post("/config/rollback/9999")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_missing_token_is_unauthorized_when_one_is_configured() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        let state = AppState::new(
            store,
            Some("secret".into()),
            RoleHandle::new(Role::Standalone),
        );
        let app = router(state);

        let resp = app
            .clone()
            .oneshot(Request::get("/config").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        let resp = app
            .oneshot(
                Request::get("/config")
                    .header("Authorization", "Bearer wrong")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_slave_tier_rejects_direct_writes() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        // Seed a revision the way a real slave would (via `apply_revision`,
        // never `submit()`) so the rollback path below has something to
        // find before it hits the role gate.
        store.put(VALID_CONFIG.as_bytes().to_vec()).unwrap();
        let state = AppState::new(store, None, RoleHandle::new(Role::Slave));
        let app = router(state);

        let resp = app
            .clone()
            .oneshot(
                Request::post("/config")
                    .body(Body::from(VALID_CONFIG))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // Rollback is a re-submission under the hood — must be rejected too.
        let resp = app
            .oneshot(
                Request::post("/config/rollback/1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn apply_revision_bypasses_the_slave_write_gate() {
        // `crate::parent_client` calls `apply_revision` directly, never
        // `submit()` — this is what lets a slave *hold* revisions while
        // still rejecting direct writes through the HTTP API.
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        let state = AppState::new(store, None, RoleHandle::new(Role::Slave));
        let revision = state.apply_revision(b"pools: []".to_vec()).unwrap();
        assert_eq!(revision, 1);
        assert_eq!(state.store.current_revision().unwrap(), Some(1));
    }

    #[tokio::test]
    async fn the_right_token_is_admitted() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        let state = AppState::new(
            store,
            Some("secret".into()),
            RoleHandle::new(Role::Standalone),
        );
        let app = router(state);

        let resp = app
            .oneshot(
                Request::get("/config")
                    .header("Authorization", "Bearer secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // No config submitted yet in this fresh store — 404, not 401, proves
        // the request got *past* the auth layer.
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
