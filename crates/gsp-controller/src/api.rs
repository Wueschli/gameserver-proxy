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

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::{Stream, StreamExt};

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
}

impl AppState {
    pub fn new(store: Arc<Store>) -> Self {
        let (updates, _rx) = broadcast::channel(UPDATES_CAPACITY);
        AppState { store, updates }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/config", post(submit_config).get(get_current_config))
        .route("/config/subscribe", get(subscribe))
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
    if let Err(e) = gsp_config::parse_str(&body) {
        tracing::warn!(error = %e, "rejected an invalid config submission");
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
            .into_response();
    }

    match state.store.put(body.into_bytes()) {
        Ok(revision) => {
            tracing::info!(revision, "accepted a new config revision");
            // No receivers (no subscriber connected right now) is not an
            // error — the revision is durably stored regardless; a
            // subscriber that connects later just gets it in its catch-up
            // range.
            let _ = state.updates.send(revision);
            (StatusCode::OK, Json(SubmitResponse { revision })).into_response()
        }
        Err(e) => store_error_response(e),
    }
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
        (AppState::new(store), dir)
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
}
