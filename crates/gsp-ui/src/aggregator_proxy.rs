//! Proxies fleet reads and the slice-9 operational verbs to `gsp-aggregator`
//! (`--aggregator-url`/`--aggregator-token`) — the browser's session cookie
//! never becomes a bearer token; `gsp-ui` holds the aggregator's token
//! itself and presents it server-side on every call. Every route here is
//! gated by [`crate::auth::require_session`] (applied by `crate::api::router`,
//! not here — matches `gsp-aggregator::fanout`'s "route definitions only"
//! shape, since this module composes into a router another module finishes
//! building).
//!
//! Thin and stateless, the same as `gsp-aggregator::fanout` it calls
//! through to: `gsp-ui` decides nothing and stores no intent, it only
//! relays.

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use serde::Serialize;

use crate::api::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/fleet/pools", get(get_pools))
        .route("/api/fleet/sessions", get(get_sessions))
        .route("/api/fleet/healthz", get(get_healthz))
        .route(
            "/api/fleet/instances/{instance}/drain",
            post(drain_instance),
        )
        .route(
            "/api/fleet/instances/{instance}/undrain",
            post(undrain_instance),
        )
        .route("/api/fleet/pools/{pool}/backends", post(add_backend))
        .route(
            "/api/fleet/pools/{pool}/backends/{addr}",
            patch(patch_backend).delete(delete_backend),
        )
        .route("/api/fleet/route-hint", post(route_hint))
}

async fn get_pools(State(state): State<AppState>) -> Response {
    proxy(&state, Method::GET, "/fleet/pools", None).await
}

async fn get_sessions(State(state): State<AppState>) -> Response {
    proxy(&state, Method::GET, "/fleet/sessions", None).await
}

async fn get_healthz(State(state): State<AppState>) -> Response {
    proxy(&state, Method::GET, "/fleet/healthz", None).await
}

async fn drain_instance(State(state): State<AppState>, Path(instance): Path<String>) -> Response {
    proxy(
        &state,
        Method::POST,
        &format!("/fleet/instances/{instance}/drain"),
        None,
    )
    .await
}

async fn undrain_instance(State(state): State<AppState>, Path(instance): Path<String>) -> Response {
    proxy(
        &state,
        Method::POST,
        &format!("/fleet/instances/{instance}/undrain"),
        None,
    )
    .await
}

async fn add_backend(
    State(state): State<AppState>,
    Path(pool): Path<String>,
    body: Bytes,
) -> Response {
    proxy(
        &state,
        Method::POST,
        &format!("/fleet/pools/{pool}/backends"),
        Some(body),
    )
    .await
}

async fn patch_backend(
    State(state): State<AppState>,
    Path((pool, addr)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    proxy(
        &state,
        Method::PATCH,
        &format!("/fleet/pools/{pool}/backends/{addr}"),
        Some(body),
    )
    .await
}

async fn delete_backend(
    State(state): State<AppState>,
    Path((pool, addr)): Path<(String, String)>,
) -> Response {
    proxy(
        &state,
        Method::DELETE,
        &format!("/fleet/pools/{pool}/backends/{addr}"),
        None,
    )
    .await
}

async fn route_hint(State(state): State<AppState>, body: Bytes) -> Response {
    proxy(&state, Method::POST, "/fleet/route-hint", Some(body)).await
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

/// Forwards one call to the configured aggregator, passing its response
/// (status + body) straight through. `503` if no `--aggregator-url` was
/// given at all — a configuration gap, not a runtime failure; `502` if the
/// aggregator was configured but couldn't be reached.
async fn proxy(
    state: &AppState,
    method: Method,
    path_suffix: &str,
    body: Option<Bytes>,
) -> Response {
    let Some(aggregator) = &state.aggregator else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "no aggregator configured (--aggregator-url)".into(),
            }),
        )
            .into_response();
    };

    let url = format!("{}{}", aggregator.base_url, path_suffix);
    let mut req = state.http.request(method, &url);
    if let Some(token) = &aggregator.token {
        req = req.bearer_auth(token);
    }
    if let Some(body) = body {
        req = req.header("content-type", "application/json").body(body);
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
                error: format!("aggregator ({url}): {e}"),
            }),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::header;
    use axum::http::Request;
    use std::sync::{Arc, Mutex};
    use tower::ServiceExt;

    /// `(request body, Authorization header)` from the last request a mock
    /// aggregator received.
    type Captured = Arc<Mutex<Option<(Vec<u8>, Option<String>)>>>;

    /// A minimal stand-in for `gsp-aggregator`: one route, a fixed response,
    /// and the last request body + auth header it received.
    async fn spawn_mock_aggregator(
        path: &'static str,
        method: Method,
        status: StatusCode,
        response_body: &'static str,
    ) -> (String, Captured) {
        let captured: Captured = Arc::new(Mutex::new(None));
        let captured_for_handler = captured.clone();
        let handler = move |headers: axum::http::HeaderMap, body: Bytes| {
            let captured = captured_for_handler.clone();
            async move {
                let auth = headers
                    .get(header::AUTHORIZATION)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                *captured.lock().unwrap() = Some((body.to_vec(), auth));
                (status, response_body)
            }
        };
        let method_router = match method {
            Method::GET => axum::routing::get(handler),
            Method::POST => axum::routing::post(handler),
            Method::PATCH => axum::routing::patch(handler),
            Method::DELETE => axum::routing::delete(handler),
            _ => unreachable!("test helper only used with GET/POST/PATCH/DELETE"),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route(path, method_router);
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), captured)
    }

    fn logged_in_app(aggregator_url: String, token: Option<String>) -> (Router, String) {
        let state =
            crate::api::AppState::new(Some("secret".into())).with_aggregator(aggregator_url, token);
        let sessions = state.sessions.clone();
        let session_id = sessions.create();
        (crate::api::router(state), session_id)
    }

    fn cookie(session_id: &str) -> String {
        format!("{}={session_id}", crate::api::SESSION_COOKIE)
    }

    #[tokio::test]
    async fn get_pools_is_gated_by_session() {
        let (url, _captured) =
            spawn_mock_aggregator("/fleet/pools", Method::GET, StatusCode::OK, "[]").await;
        let (app, _session_id) = logged_in_app(url, None);

        let resp = app
            .oneshot(
                Request::get("/api/fleet/pools")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn get_pools_proxies_to_the_aggregator_with_its_token() {
        let (url, captured) = spawn_mock_aggregator(
            "/fleet/pools",
            Method::GET,
            StatusCode::OK,
            r#"[{"instance":"a"}]"#,
        )
        .await;
        let (app, session_id) = logged_in_app(url, Some("agg-token".into()));

        let resp = app
            .oneshot(
                Request::get("/api/fleet/pools")
                    .header(header::COOKIE, cookie(&session_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(bytes, r#"[{"instance":"a"}]"#.as_bytes());

        let (_, auth) = captured.lock().unwrap().clone().unwrap();
        assert_eq!(auth.as_deref(), Some("Bearer agg-token"));
    }

    #[tokio::test]
    async fn add_backend_forwards_the_body_and_the_aggregators_status() {
        let (url, captured) = spawn_mock_aggregator(
            "/fleet/pools/local/backends",
            Method::POST,
            StatusCode::OK,
            r#"{"results":[]}"#,
        )
        .await;
        let (app, session_id) = logged_in_app(url, None);

        let resp = app
            .oneshot(
                Request::post("/api/fleet/pools/local/backends")
                    .header(header::COOKIE, cookie(&session_id))
                    .body(Body::from(r#"{"addr":"127.0.0.1:9001"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let (body, _) = captured.lock().unwrap().clone().unwrap();
        assert_eq!(body, r#"{"addr":"127.0.0.1:9001"}"#.as_bytes());
    }

    #[tokio::test]
    async fn drain_instance_hits_the_targeted_path() {
        let (url, _captured) = spawn_mock_aggregator(
            "/fleet/instances/proxy-1/drain",
            Method::POST,
            StatusCode::OK,
            "draining\n",
        )
        .await;
        let (app, session_id) = logged_in_app(url, None);

        let resp = app
            .oneshot(
                Request::post("/api/fleet/instances/proxy-1/drain")
                    .header(header::COOKIE, cookie(&session_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn no_aggregator_configured_is_a_clean_503_not_a_panic() {
        let state = crate::api::AppState::new(None);
        let app = crate::api::router(state);
        let resp = app
            .oneshot(
                Request::get("/api/fleet/pools")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn an_unreachable_aggregator_is_a_clean_502_not_a_panic() {
        let state =
            crate::api::AppState::new(None).with_aggregator("http://127.0.0.1:1".to_string(), None);
        let sessions = state.sessions.clone();
        let session_id = sessions.create();
        let app = crate::api::router(state);

        let resp = app
            .oneshot(
                Request::get("/api/fleet/pools")
                    .header(header::COOKIE, cookie(&session_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    }
}
