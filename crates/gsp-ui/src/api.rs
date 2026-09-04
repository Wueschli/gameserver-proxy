//! `POST /ui/login` / `POST /ui/logout` / `GET /ui/session` (slice 11b) —
//! the browser's own session-cookie auth. Wholly separate from every
//! machine-to-machine bearer token this fleet uses (`gsp-controller`'s /
//! `gsp-aggregator`'s `--auth-token`, `--instance-token`,
//! `--aggregator-token`, `settings.admin.auth_token`) — `gsp-ui` is the only
//! thing that ever holds those, on the browser's behalf (`docs/10` "The
//! admin GUI").
//!
//! A single shared `--ui-password`, not per-user accounts — matches the
//! "single shared secret, not RBAC" posture every other token in this
//! release already has. `None` leaves the UI open (no login required),
//! consistent with every other optional-auth surface in this fleet.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::session::SessionStore;

/// The cookie the browser holds. `HttpOnly` (never readable from JS) +
/// `SameSite=Lax`. **Not marked `Secure`** — a PoC deployment commonly runs
/// over plain HTTP (localhost, an internal network); a real TLS-fronted
/// deployment should add it. Tracked as a known gap, not silently ignored.
pub const SESSION_COOKIE: &str = "gsp_ui_session";

/// Where `crate::aggregator_proxy` sends its calls, and the bearer token it
/// presents there — a separate secret from `gsp-controller`'s (`docs/10`:
/// `gsp-ui` holds each service's own credential, never conflates them).
#[derive(Clone)]
pub struct AggregatorTarget {
    pub base_url: String,
    pub token: Option<String>,
}

#[derive(Clone)]
pub struct AppState {
    pub ui_password: Option<Arc<str>>,
    pub sessions: Arc<SessionStore>,
    /// Shared client for proxying out to the aggregator (slice 11c) and,
    /// later, the controller (slice 11e). Cheap to clone (an `Arc`
    /// internally), reuses connections.
    pub http: reqwest::Client,
    pub aggregator: Option<AggregatorTarget>,
    /// The live bridge to the aggregator's `/fleet/subscribe` feed
    /// ([`crate::fleet_feed`], slice 11d) — `None` when no aggregator is
    /// configured at all, same as `aggregator` being `None`.
    pub fleet_feed: Option<std::sync::Arc<crate::fleet_feed::FleetFeed>>,
}

impl AppState {
    pub fn new(ui_password: Option<String>) -> Self {
        AppState {
            ui_password: ui_password.map(Arc::from),
            sessions: Arc::new(SessionStore::new()),
            http: reqwest::Client::new(),
            aggregator: None,
            fleet_feed: None,
        }
    }

    pub fn with_aggregator(mut self, base_url: String, token: Option<String>) -> Self {
        self.aggregator = Some(AggregatorTarget { base_url, token });
        self
    }

    pub fn with_fleet_feed(mut self, feed: std::sync::Arc<crate::fleet_feed::FleetFeed>) -> Self {
        self.fleet_feed = Some(feed);
        self
    }
}

/// `/ui/login` and `/ui/logout` must be reachable *without* a session (that
/// would be circular); everything else this process serves — `/ui/session`,
/// `crate::aggregator_proxy`'s routes, and `crate::ws`'s WebSocket — is
/// gated by [`crate::auth::require_session`].
pub fn router(state: AppState) -> Router {
    let gated = Router::new()
        .route("/ui/session", get(session_status))
        .merge(crate::aggregator_proxy::router())
        .merge(crate::ws::router())
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::auth::require_session,
        ));

    Router::new()
        .route("/ui/login", post(login))
        .route("/ui/logout", post(logout))
        .merge(gated)
        .with_state(state)
}

#[derive(Deserialize)]
struct LoginRequest {
    password: String,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

/// `POST /ui/login {"password": "..."}` — issues a session cookie on
/// success. With no `--ui-password` configured, login always succeeds (the
/// UI is open) but still issues a session, so `require_session` behaves
/// uniformly either way rather than needing a special case.
async fn login(State(state): State<AppState>, Json(req): Json<LoginRequest>) -> Response {
    match state.ui_password.as_deref() {
        Some(expected) if req.password != expected => (
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse {
                error: "wrong password".into(),
            }),
        )
            .into_response(),
        _ => issue_session(&state),
    }
}

fn issue_session(state: &AppState) -> Response {
    let id = state.sessions.create();
    let cookie = format!("{SESSION_COOKIE}={id}; HttpOnly; Path=/; SameSite=Lax");
    (StatusCode::OK, [(header::SET_COOKIE, cookie)], "ok").into_response()
}

/// `POST /ui/logout` — revokes the session named by the request's cookie, if
/// any, and tells the browser to drop it. Never gated by `require_session`
/// itself: logging out an already-invalid/missing session is a no-op, not an
/// error.
async fn logout(State(state): State<AppState>, req: Request) -> Response {
    if let Some(id) = session_id_from(&req) {
        state.sessions.revoke(&id);
    }
    let expire_cookie = format!("{SESSION_COOKIE}=; HttpOnly; Path=/; SameSite=Lax; Max-Age=0");
    (StatusCode::OK, [(header::SET_COOKIE, expire_cookie)], "ok").into_response()
}

/// `GET /ui/session` — gated by [`crate::auth::require_session`]; reaching
/// the handler at all means the session was valid (or no password is
/// configured). Lets the frontend check "am I logged in" on load without a
/// dedicated no-op probe.
async fn session_status() -> Response {
    (
        StatusCode::OK,
        Json(serde_json::json!({ "authenticated": true })),
    )
        .into_response()
}

/// Extracts the session id from the `Cookie` header, if present. Shared by
/// [`logout`] and [`crate::auth::require_session`].
pub fn session_id_from(req: &Request) -> Option<String> {
    let cookie_header = req.headers().get(header::COOKIE)?.to_str().ok()?;
    let prefix = format!("{SESSION_COOKIE}=");
    cookie_header
        .split(';')
        .map(str::trim)
        .find_map(|part| part.strip_prefix(&prefix).map(str::to_string))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use tower::ServiceExt;

    fn set_cookie_value(resp: &axum::response::Response) -> String {
        resp.headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string()
    }

    fn cookie_header_from(set_cookie: &str) -> String {
        // "gsp_ui_session=<id>; HttpOnly; ..." -> "gsp_ui_session=<id>"
        set_cookie.split(';').next().unwrap().to_string()
    }

    #[tokio::test]
    async fn login_with_no_password_configured_always_succeeds() {
        let app = router(AppState::new(None));
        let resp = app
            .oneshot(
                HttpRequest::post("/ui/login")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"password":"anything"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(resp.headers().contains_key(header::SET_COOKIE));
    }

    #[tokio::test]
    async fn wrong_password_is_rejected() {
        let app = router(AppState::new(Some("secret".into())));
        let resp = app
            .oneshot(
                HttpRequest::post("/ui/login")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"password":"wrong"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn the_right_password_grants_a_session_that_unlocks_gated_routes() {
        let app = router(AppState::new(Some("secret".into())));

        // Gated route rejects with no cookie at all.
        let resp = app
            .clone()
            .oneshot(HttpRequest::get("/ui/session").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        let resp = app
            .clone()
            .oneshot(
                HttpRequest::post("/ui/login")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"password":"secret"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let cookie = cookie_header_from(&set_cookie_value(&resp));

        let resp = app
            .oneshot(
                HttpRequest::get("/ui/session")
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn logout_revokes_the_session_so_it_no_longer_unlocks_gated_routes() {
        let app = router(AppState::new(Some("secret".into())));

        let resp = app
            .clone()
            .oneshot(
                HttpRequest::post("/ui/login")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"password":"secret"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let cookie = cookie_header_from(&set_cookie_value(&resp));

        app.clone()
            .oneshot(
                HttpRequest::post("/ui/logout")
                    .header(header::COOKIE, cookie.clone())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let resp = app
            .oneshot(
                HttpRequest::get("/ui/session")
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_garbage_cookie_is_rejected_not_a_panic() {
        let app = router(AppState::new(Some("secret".into())));
        let resp = app
            .oneshot(
                HttpRequest::get("/ui/session")
                    .header(header::COOKIE, "gsp_ui_session=not-a-real-session")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}
