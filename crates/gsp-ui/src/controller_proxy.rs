//! Proxies `gsp-controller`'s config API — `GET`/`POST /config`, revision
//! history/diff, rollback, promote — and its read-only tunnel address table
//! (`GET /tunnel/addresses`, `docs/11` "Address authority", plus the registry
//! `DELETE`s that release an address) to the browser, via
//! `--controller-url`/`--controller-token`. Same shape as
//! `crate::aggregator_proxy`: thin, stateless, the browser's session cookie
//! never becomes a bearer token, `gsp-ui` holds the controller's own
//! credential and presents it server-side. [`viewer_router`]'s reads are
//! `Role::Viewer`; [`admin_router`]'s writes (submit, rollback, promote,
//! release —
//! phase 10's "full management" GUI level, `docs/10`) are `Role::Admin`.
//!
//! The controller's `POST /config` body is raw YAML text, not JSON (it
//! accepts a `String` extractor, content-type-agnostic) — forwarded
//! byte-for-byte with no content-type forced on it, unlike
//! `aggregator_proxy`'s JSON bodies. [`admin_router`]'s writes each add an
//! `X-Actor` header (phase 12 slice 8) naming the session's username, if
//! any — `gsp-controller` records it per revision (`GET /config/revisions`'
//! `actor` field), the durable half of this release's audit trail.

use axum::body::Bytes;
use axum::extract::{Extension, Path, RawQuery, State};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Serialize;

use crate::api::AppState;
use crate::auth::Actor;

/// `GET` config reads — `Role::Viewer`.
pub fn viewer_router() -> Router<AppState> {
    Router::new()
        .route("/api/config", get(get_config))
        .route("/api/config/revisions", get(list_revisions))
        .route("/api/config/revisions/{revision}", get(get_revision))
        .route("/api/config/revisions/{revision}/diff", get(diff_revision))
        .route("/api/tunnel/addresses", get(tunnel_addresses))
}

/// Config-changing writes — `Role::Admin`.
pub fn admin_router() -> Router<AppState> {
    Router::new()
        .route("/api/config", post(submit_config))
        .route("/api/config/rollback/{revision}", post(rollback))
        .route("/api/config/promote/{revision}", post(promote))
        .route("/api/tunnel/origins/{name}", delete(release_origin))
        .route("/api/tunnel/proxies/{name}", delete(release_proxy))
}

async fn get_config(State(state): State<AppState>) -> Response {
    proxy(&state, Method::GET, "/config".to_string(), None, None).await
}

async fn tunnel_addresses(State(state): State<AppState>) -> Response {
    proxy(
        &state,
        Method::GET,
        "/tunnel/addresses".to_string(),
        None,
        None,
    )
    .await
}

async fn submit_config(
    State(state): State<AppState>,
    Extension(Actor(actor)): Extension<Actor>,
    RawQuery(query): RawQuery,
    body: Bytes,
) -> Response {
    let suffix = match query {
        Some(q) => format!("/config?{q}"),
        None => "/config".to_string(),
    };
    proxy(&state, Method::POST, suffix, Some(body), actor).await
}

async fn list_revisions(State(state): State<AppState>) -> Response {
    proxy(
        &state,
        Method::GET,
        "/config/revisions".to_string(),
        None,
        None,
    )
    .await
}

async fn get_revision(State(state): State<AppState>, Path(revision): Path<u64>) -> Response {
    proxy(
        &state,
        Method::GET,
        format!("/config/revisions/{revision}"),
        None,
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
    proxy(&state, Method::GET, suffix, None, None).await
}

async fn rollback(
    State(state): State<AppState>,
    Extension(Actor(actor)): Extension<Actor>,
    Path(revision): Path<u64>,
) -> Response {
    proxy(
        &state,
        Method::POST,
        format!("/config/rollback/{revision}"),
        None,
        actor,
    )
    .await
}

async fn promote(
    State(state): State<AppState>,
    Extension(Actor(actor)): Extension<Actor>,
    Path(revision): Path<u64>,
) -> Response {
    proxy(
        &state,
        Method::POST,
        format!("/config/promote/{revision}"),
        None,
        actor,
    )
    .await
}

/// `DELETE /peers/{name}` — releases an origin's tunnel address.
async fn release_origin(
    State(state): State<AppState>,
    Extension(Actor(actor)): Extension<Actor>,
    Path(name): Path<String>,
) -> Response {
    release(&state, "peers", &name, actor).await
}

/// `DELETE /proxy-peers/{name}` — releases a proxy's tunnel address.
async fn release_proxy(
    State(state): State<AppState>,
    Extension(Actor(actor)): Extension<Actor>,
    Path(name): Path<String>,
) -> Response {
    release(&state, "proxy-peers", &name, actor).await
}

/// `Path` has already percent-decoded `name`; re-encode it so it stays one
/// path segment, and refuse the dot segments a URL normalizer would resolve
/// away from the registry route.
async fn release(state: &AppState, base: &str, name: &str, actor: Option<String>) -> Response {
    if name == "." || name == ".." {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "invalid registration name".into(),
            }),
        )
            .into_response();
    }
    let mut segment = String::with_capacity(name.len());
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            segment.push(b as char);
        } else {
            segment.push_str(&format!("%{b:02X}"));
        }
    }
    proxy(
        state,
        Method::DELETE,
        format!("/{base}/{segment}"),
        None,
        actor,
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
    actor: Option<String>,
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
    if let Some(actor) = actor {
        req = req.header("X-Actor", actor);
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
                error: format!("controller ({url}): {}", gsp_http::error_chain(&e)),
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
    async fn tunnel_addresses_proxies_to_the_controller_with_the_token() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mock = Router::new().route(
            "/tunnel/addresses",
            get(|headers: axum::http::HeaderMap| async move {
                assert_eq!(
                    headers.get(axum::http::header::AUTHORIZATION).unwrap(),
                    "Bearer ctl-token"
                );
                r#"{"network":"10.200.0.0/24","allocated":0,"capacity":254,"entries":[]}"#
            }),
        );
        tokio::spawn(async move {
            axum::serve(listener, mock).await.unwrap();
        });

        let app = app_with_controller(format!("http://{addr}"), Some("ctl-token".into()));
        let resp = app
            .oneshot(
                Request::get("/api/tunnel/addresses")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("10.200.0.0/24"));
    }

    #[tokio::test]
    async fn release_proxies_a_registry_delete_with_the_token() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mock = Router::new()
            .route(
                "/peers/{name}",
                axum::routing::delete(
                    |headers: axum::http::HeaderMap, Path(name): Path<String>| async move {
                        assert_eq!(
                            headers.get(axum::http::header::AUTHORIZATION).unwrap(),
                            "Bearer ctl-token"
                        );
                        format!(r#"{{"revision":4,"released":"{name}"}}"#)
                    },
                ),
            )
            .route(
                "/proxy-peers/{name}",
                axum::routing::delete(|| async { (StatusCode::NOT_FOUND, "no such proxy") }),
            );
        tokio::spawn(async move {
            axum::serve(listener, mock).await.unwrap();
        });
        let app = app_with_controller(format!("http://{addr}"), Some("ctl-token".into()));

        let resp = app
            .clone()
            .oneshot(
                Request::delete("/api/tunnel/origins/game-1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("game-1"));

        // The controller's status passes straight through (a 404 stays a 404).
        let resp = app
            .oneshot(
                Request::delete("/api/tunnel/proxies/p1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn release_cannot_be_steered_to_another_controller_path() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Anything but the registry route would be a DELETE on /config.
        let mock = Router::new().fallback(|uri: axum::http::Uri| async move {
            assert!(uri.path().starts_with("/peers/"), "escaped to {uri}");
            "{}"
        });
        tokio::spawn(async move {
            axum::serve(listener, mock).await.unwrap();
        });
        let app = app_with_controller(format!("http://{addr}"), None);

        for name in ["..", "%2E%2E", "..%2Fconfig", "a%2Fb", "a%3Fb"] {
            let resp = app
                .clone()
                .oneshot(
                    Request::delete(format!("/api/tunnel/origins/{name}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert!(
                resp.status() == StatusCode::BAD_REQUEST || resp.status() == StatusCode::OK,
                "{name}: {}",
                resp.status()
            );
        }
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
