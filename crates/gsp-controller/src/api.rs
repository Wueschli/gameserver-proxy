//! `POST /config` / `GET /config` — the config submit API (slice 2). Every
//! submission runs the **same `gsp_config::parse_str`** (parse + `validate()`)
//! a proxy runs on a file reload before it ever reaches [`Store::put`]; a
//! rejected submission never touches the store, so the previous revision
//! stays current — the same "bad reload keeps the old snapshot" rule the
//! proxy already has, one hop earlier.
//!
//! `GET /config` returns the current revision's raw text. Revision history /
//! diff / rollback (slice 5) build on top of the same [`Store`].

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::Serialize;

use crate::store::{Store, StoreError};

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Store>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/config", post(submit_config).get(get_current_config))
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
        (AppState { store }, dir)
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
}
