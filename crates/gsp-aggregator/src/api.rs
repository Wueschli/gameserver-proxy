//! `POST /ingest` (slice 6) — a proxy pushes its own [`IngestPayload`]
//! periodically; the aggregator stores latest-write-wins per `instance` and
//! (slice 8) serves a merged fleet view over the same [`IngestStore`]. See
//! the module doc in `lib.rs` for why this is push, not pull.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::Serialize;

use crate::ingest::{IngestPayload, IngestStore};

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<IngestStore>,
}

impl AppState {
    pub fn new(store: Arc<IngestStore>) -> Self {
        AppState { store }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/ingest", post(ingest))
        .with_state(state)
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

/// `POST /ingest` — body is one [`IngestPayload`] as JSON. `instance` must
/// be non-empty (it's the key everything is stored and overwritten under);
/// anything else in the payload is accepted as-is, this is a summary a proxy
/// self-reports, not something the aggregator validates against reality.
async fn ingest(State(state): State<AppState>, Json(payload): Json<IngestPayload>) -> Response {
    if payload.instance.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "instance must not be empty".into(),
            }),
        )
            .into_response();
    }
    let instance = payload.instance.clone();
    state.store.ingest(payload);
    tracing::debug!(instance, "ingested a push from a proxy instance");
    StatusCode::OK.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::SessionCounts;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn test_state() -> AppState {
        AppState::new(Arc::new(IngestStore::new()))
    }

    fn payload_json(instance: &str) -> String {
        serde_json::to_string(&IngestPayload {
            instance: instance.to_string(),
            pools: vec![],
            sessions: SessionCounts { tcp: 3, udp: 7 },
        })
        .unwrap()
    }

    #[tokio::test]
    async fn a_valid_push_is_accepted_and_stored() {
        let state = test_state();
        let store = state.store.clone();
        let app = router(state);

        let resp = app
            .oneshot(
                Request::post("/ingest")
                    .header("content-type", "application/json")
                    .body(Body::from(payload_json("proxy-1")))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let stored = store.get("proxy-1").unwrap();
        assert_eq!(stored.payload.sessions.tcp, 3);
        assert_eq!(stored.payload.sessions.udp, 7);
    }

    #[tokio::test]
    async fn an_empty_instance_name_is_rejected() {
        let app = router(test_state());
        let resp = app
            .oneshot(
                Request::post("/ingest")
                    .header("content-type", "application/json")
                    .body(Body::from(payload_json("  ")))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn malformed_json_is_rejected_by_the_extractor() {
        let app = router(test_state());
        let resp = app
            .oneshot(
                Request::post("/ingest")
                    .header("content-type", "application/json")
                    .body(Body::from("not json"))
                    .unwrap(),
            )
            .await
            .unwrap();
        // axum's `Json` extractor rejects a body that doesn't even parse as
        // JSON with 400 (a 422 would be a body that parses but fails
        // `IngestPayload`'s shape) — this never reaches our own handler.
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn a_second_push_from_the_same_instance_overwrites_the_first() {
        let state = test_state();
        let store = state.store.clone();
        let app = router(state);

        app.clone()
            .oneshot(
                Request::post("/ingest")
                    .header("content-type", "application/json")
                    .body(Body::from(payload_json("proxy-1")))
                    .unwrap(),
            )
            .await
            .unwrap();

        let mut second = serde_json::from_str::<IngestPayload>(&payload_json("proxy-1")).unwrap();
        second.sessions = SessionCounts { tcp: 99, udp: 0 };
        app.oneshot(
            Request::post("/ingest")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&second).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

        assert_eq!(store.snapshot().len(), 1);
        assert_eq!(store.get("proxy-1").unwrap().payload.sessions.tcp, 99);
    }
}
