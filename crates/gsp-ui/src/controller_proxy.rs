//! Proxies `gsp-controller`'s config API — `GET`/`POST /config`, revision
//! history/diff, rollback — to the browser, via `--controller-url`/
//! `--controller-token`. Same shape as `crate::aggregator_proxy`: thin,
//! stateless, the browser's session cookie never becomes a bearer token,
//! `gsp-ui` holds the controller's own credential and presents it
//! server-side. This is phase 10's "full management" GUI capability level
//! (`docs/10`) — structural config editing, on top of slice 11c's
//! operational level.
//!
//! The controller's `POST /config` body is raw YAML text, not JSON (it
//! accepts a `String` extractor, content-type-agnostic) — forwarded
//! byte-for-byte with no content-type forced on it, unlike
//! `aggregator_proxy`'s JSON bodies.

use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;

use crate::api::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/config", get(get_config).post(submit_config))
        .route("/api/config/revisions", get(list_revisions))
        .route("/api/config/revisions/{revision}", get(get_revision))
        .route("/api/config/revisions/{revision}/diff", get(diff_revision))
        .route("/api/config/rollback/{revision}", post(rollback))
}

async fn get_config(State(state): State<AppState>) -> Response {
    proxy(&state, Method::GET, "/config".to_string(), None).await
}

async fn submit_config(State(state): State<AppState>, body: Bytes) -> Response {
    proxy(&state, Method::POST, "/config".to_string(), Some(body)).await
}

async fn list_revisions(State(state): State<AppState>) -> Response {
    proxy(&state, Method::GET, "/config/revisions".to_string(), None).await
}

async fn get_revision(State(state): State<AppState>, Path(revision): Path<u64>) -> Response {
    proxy(
        &state,
        Method::GET,
        format!("/config/revisions/{revision}"),
        None,
    )
    .await
}

async fn diff_revision(
    State(state): State<AppState>,
    Path(revision): Path<u64>,
    RawQuery(query): RawQuery,
) -> Response {
    let suffix = match query {
        Some(q) => format!("/config/revisions/{revision}/diff?{q}"),
        None => format!("/config/revisions/{revision}/diff"),
    };
    proxy(&state, Method::GET, suffix, None).await
}

async fn rollback(State(state): State<AppState>, Path(revision): Path<u64>) -> Response {
    proxy(
        &state,
        Method::POST,
        format!("/config/rollback/{revision}"),
        None,
    )
    .await
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

/// Forwards one call to the configured controller, passing its response
/// (status + body) straight through. `503` if no `--controller-url` was
/// given at all; `502` if it was but couldn't be reached — both handled
/// cleanly, matching `aggregator_proxy::proxy`'s shape exactly.
async fn proxy(
    state: &AppState,
    method: Method,
    path_suffix: String,
    body: Option<Bytes>,
) -> Response {
    let Some(controller) = &state.controller else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "no controller configured (--controller-url)".into(),
            }),
        )
            .into_response();
    };

    let url = format!("{}{}", controller.base_url, path_suffix);
    let mut req = state.http.request(method, &url);
    if let Some(token) = &controller.token {
        req = req.bearer_auth(token);
    }
    if let Some(body) = body {
        req = req.body(body); // raw YAML text — no content-type forced
    }

    match req.send().await {
        Ok(resp) => {
            let status = resp.status();
            let headers = crate::proxy_util::forwardable_headers(resp.headers());
            let body = resp.bytes().await.unwrap_or_default();
            (status, headers, body).into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(ErrorResponse {
                error: format!("controller ({url}): {e}"),
            }),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn app_with_controller(url: String, token: Option<String>) -> Router {
        let state = crate::api::AppState::new(None).with_controller(url, token);
        crate::api::router(state)
    }

    #[tokio::test]
    async fn get_config_proxies_to_the_controller() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mock = Router::new().route(
            "/config",
            get(|| async { ([("x-config-revision", "3")], "pools: []") }),
        );
        tokio::spawn(async move {
            axum::serve(listener, mock).await.unwrap();
        });

        let app = app_with_controller(format!("http://{addr}"), None);
        let resp = app
            .oneshot(Request::get("/api/config").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers().get("x-config-revision").unwrap(), "3");
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(bytes, "pools: []".as_bytes());
    }

    #[tokio::test]
    async fn submit_config_forwards_the_raw_body_with_the_token() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let captured = std::sync::Arc::new(std::sync::Mutex::new(None));
        let captured_for_handler = captured.clone();
        let mock = Router::new().route(
            "/config",
            post(move |headers: axum::http::HeaderMap, body: Bytes| {
                let captured = captured_for_handler.clone();
                async move {
                    let auth = headers
                        .get(axum::http::header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string);
                    *captured.lock().unwrap() = Some((body.to_vec(), auth));
                    (StatusCode::OK, r#"{"revision":1}"#)
                }
            }),
        );
        tokio::spawn(async move {
            axum::serve(listener, mock).await.unwrap();
        });

        let app = app_with_controller(format!("http://{addr}"), Some("ctl-token".into()));
        let resp = app
            .oneshot(
                Request::post("/api/config")
                    .body(Body::from("pools:\n  - name: p\n    targets: []\n"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let (body, auth) = captured.lock().unwrap().clone().unwrap();
        assert_eq!(body, b"pools:\n  - name: p\n    targets: []\n");
        assert_eq!(auth.as_deref(), Some("Bearer ctl-token"));
    }

    #[tokio::test]
    async fn diff_forwards_the_query_string() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let captured = std::sync::Arc::new(std::sync::Mutex::new(None));
        let captured_for_handler = captured.clone();
        let mock = Router::new().route(
            "/config/revisions/{revision}/diff",
            get(
                move |Path(revision): Path<u64>, RawQuery(query): RawQuery| {
                    let captured = captured_for_handler.clone();
                    async move {
                        *captured.lock().unwrap() = Some((revision, query));
                        "diff text"
                    }
                },
            ),
        );
        tokio::spawn(async move {
            axum::serve(listener, mock).await.unwrap();
        });

        let app = app_with_controller(format!("http://{addr}"), None);
        let resp = app
            .oneshot(
                Request::get("/api/config/revisions/1/diff?against=2")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            captured.lock().unwrap().clone().unwrap(),
            (1, Some("against=2".to_string()))
        );
    }

    #[tokio::test]
    async fn rollback_hits_the_right_path() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mock = Router::new().route(
            "/config/rollback/{revision}",
            post(|Path(revision): Path<u64>| async move {
                assert_eq!(revision, 5);
                r#"{"revision":6}"#
            }),
        );
        tokio::spawn(async move {
            axum::serve(listener, mock).await.unwrap();
        });

        let app = app_with_controller(format!("http://{addr}"), None);
        let resp = app
            .oneshot(
                Request::post("/api/config/rollback/5")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn no_controller_configured_is_a_clean_503() {
        let state = crate::api::AppState::new(None);
        let app = crate::api::router(state);
        let resp = app
            .oneshot(Request::get("/api/config").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
