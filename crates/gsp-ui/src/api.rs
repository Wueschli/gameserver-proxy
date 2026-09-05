//! `POST /ui/login` / `POST /ui/logout` / `GET /ui/session` (slice 11b) —
//! the browser's own session-cookie auth. Wholly separate from every
//! machine-to-machine bearer token this fleet uses (`gsp-controller`'s /
//! `gsp-aggregator`'s `--auth-token`, `--instance-token`,
//! `--aggregator-token`, `settings.admin.auth_token`) — `gsp-ui` is the only
//! thing that ever holds those, on the browser's behalf (`docs/10` "The
//! admin GUI").
//!
//! Phase 12 slice 8 (`docs/10` "RBAC and audit (design)") adds
//! multi-operator accounts on top of the single shared `--ui-password`:
//! `--users-file` (`crate::users`) maps a login to one of three
//! [`crate::role::Role`]s, enforced per route group by
//! [`crate::auth::check_role`]. **`--ui-password` is kept, not replaced** —
//! the one place this codebase's usual "clean break over compat shim"
//! pre-1.0 stance doesn't apply: forcing every existing single-operator
//! deployment to mint a `users-file` for one identity is pure friction with
//! no correctness upside, since RBAC only matters once there's more than
//! one identity to distinguish. A legacy `--ui-password` session is
//! implicitly `Role::Admin`; with neither configured, the UI is fully open
//! (also implicitly `Admin`), matching every other optional-auth surface in
//! this fleet.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::role::Role;
use crate::session::{Session, SessionStore};
use crate::users::UserRecord;

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

/// Where `crate::controller_proxy` sends its calls, and the bearer token it
/// presents there — a separate secret from `gsp-aggregator`'s, same
/// reasoning as [`AggregatorTarget`].
#[derive(Clone)]
pub struct ControllerTarget {
    pub base_url: String,
    pub token: Option<String>,
}

#[derive(Clone)]
pub struct AppState {
    /// Legacy single-shared-secret mode — see the module doc. Mutually
    /// exclusive with `users` (`main.rs` rejects both being set).
    pub ui_password: Option<Arc<str>>,
    /// `--users-file` mode, keyed by username. `None` means "not in
    /// multi-user mode" (either `ui_password` mode, or fully open).
    pub users: Option<Arc<HashMap<String, UserRecord>>>,
    pub sessions: Arc<SessionStore>,
    /// Shared client for proxying out to the aggregator (slice 11c) and the
    /// controller (slice 11e). Cheap to clone (an `Arc` internally), reuses
    /// connections.
    pub http: reqwest::Client,
    pub aggregator: Option<AggregatorTarget>,
    pub controller: Option<ControllerTarget>,
    /// The live bridge to the aggregator's `/fleet/subscribe` feed
    /// ([`crate::fleet_feed`], slice 11d) — `None` when no aggregator is
    /// configured at all, same as `aggregator` being `None`.
    pub fleet_feed: Option<std::sync::Arc<crate::fleet_feed::FleetFeed>>,
}

impl AppState {
    pub fn new(ui_password: Option<String>) -> Self {
        AppState {
            ui_password: ui_password.map(Arc::from),
            users: None,
            sessions: Arc::new(SessionStore::new()),
            http: reqwest::Client::new(),
            aggregator: None,
            controller: None,
            fleet_feed: None,
        }
    }

    pub fn with_users(mut self, users: HashMap<String, UserRecord>) -> Self {
        self.users = Some(Arc::new(users));
        self
    }

    pub fn with_aggregator(mut self, base_url: String, token: Option<String>) -> Self {
        self.aggregator = Some(AggregatorTarget { base_url, token });
        self
    }

    pub fn with_controller(mut self, base_url: String, token: Option<String>) -> Self {
        self.controller = Some(ControllerTarget { base_url, token });
        self
    }

    pub fn with_fleet_feed(mut self, feed: std::sync::Arc<crate::fleet_feed::FleetFeed>) -> Self {
        self.fleet_feed = Some(feed);
        self
    }

    /// `false` means every request is treated as an anonymous `Role::Admin`
    /// (`crate::auth::check_role`) — the fully-open posture every other
    /// optional-auth surface in this fleet has with no token configured.
    pub fn auth_configured(&self) -> bool {
        self.ui_password.is_some() || self.users.is_some()
    }
}

/// `/ui/login` and `/ui/logout` must be reachable *without* a session (that
/// would be circular). Everything else is gated by
/// [`crate::auth::check_role`] at one of three levels: `Viewer`
/// (`/ui/session`, every read), `Operator` (the phase-5 intent verbs,
/// `crate::aggregator_proxy::operator_router`), `Admin` (config
/// submit/rollback/promote, `crate::controller_proxy::admin_router`). Each
/// level gets its own `route_layer` on its own sub-router before merging,
/// rather than one shared gate — `route_layer` only applies to routes
/// already added to the `Router` it's called on, so this is the natural way
/// to give three route groups three different minimums without any
/// ordering subtlety between stacked layers.
pub fn router(state: AppState) -> Router {
    macro_rules! role_layer {
        ($min:expr) => {
            axum::middleware::from_fn_with_state(
                state.clone(),
                move |State(s): State<AppState>, req: Request, next: axum::middleware::Next| async move {
                    crate::auth::check_role(s, req, next, $min).await
                },
            )
        };
    }

    let viewer = Router::new()
        .route("/ui/session", get(session_status))
        .merge(crate::aggregator_proxy::viewer_router())
        .merge(crate::controller_proxy::viewer_router())
        .merge(crate::ws::router())
        .route_layer(role_layer!(Role::Viewer));

    let operator =
        crate::aggregator_proxy::operator_router().route_layer(role_layer!(Role::Operator));

    let admin = crate::controller_proxy::admin_router().route_layer(role_layer!(Role::Admin));

    Router::new()
        .route("/ui/login", post(login))
        .route("/ui/logout", post(logout))
        .merge(viewer)
        .merge(operator)
        .merge(admin)
        .with_state(state)
}

#[derive(Deserialize)]
struct LoginRequest {
    /// Required in `--users-file` mode; ignored in legacy `--ui-password`
    /// mode (there's only ever one identity there).
    #[serde(default)]
    username: Option<String>,
    password: String,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

/// `POST /ui/login {"username": "...", "password": "..."}` (`username`
/// omitted in legacy mode) — issues a session cookie on success. With
/// neither `--users-file` nor `--ui-password` configured, login always
/// succeeds (the UI is open) but still issues an (anonymous, `Admin`)
/// session, so `check_role` behaves uniformly either way rather than
/// needing a special case.
async fn login(State(state): State<AppState>, Json(req): Json<LoginRequest>) -> Response {
    if let Some(users) = &state.users {
        let Some(username) = req.username.as_deref() else {
            return bad_credentials();
        };
        let Some(user) = users.get(username) else {
            return bad_credentials();
        };
        if !crate::users::verify_password(&user.password_hash, &req.password) {
            return bad_credentials();
        }
        return issue_session(
            &state,
            Session {
                role: user.role,
                username: Some(user.username.clone()),
            },
        );
    }

    match state.ui_password.as_deref() {
        Some(expected) if req.password != expected => bad_credentials(),
        _ => issue_session(
            &state,
            Session {
                role: Role::Admin,
                username: None,
            },
        ),
    }
}

fn bad_credentials() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(ErrorResponse {
            error: "wrong username or password".into(),
        }),
    )
        .into_response()
}

fn issue_session(state: &AppState, session: Session) -> Response {
    let id = state.sessions.create(session);
    let cookie = format!("{SESSION_COOKIE}={id}; HttpOnly; Path=/; SameSite=Lax");
    (StatusCode::OK, [(header::SET_COOKIE, cookie)], "ok").into_response()
}

/// `POST /ui/logout` — revokes the session named by the request's cookie, if
/// any, and tells the browser to drop it. Never gated: logging out an
/// already-invalid/missing session is a no-op, not an error.
async fn logout(State(state): State<AppState>, req: Request) -> Response {
    if let Some(id) = session_id_from(&req) {
        state.sessions.revoke(&id);
    }
    let expire_cookie = format!("{SESSION_COOKIE}=; HttpOnly; Path=/; SameSite=Lax; Max-Age=0");
    (StatusCode::OK, [(header::SET_COOKIE, expire_cookie)], "ok").into_response()
}

/// `GET /ui/session` — gated at `Role::Viewer`; reaching the handler at all
/// means the session was valid (or no auth is configured). Lets the
/// frontend check "am I logged in, and what can I do" on load without a
/// dedicated no-op probe.
async fn session_status(
    axum::Extension(crate::auth::Actor(username)): axum::Extension<crate::auth::Actor>,
) -> Response {
    (
        StatusCode::OK,
        Json(serde_json::json!({ "authenticated": true, "username": username })),
    )
        .into_response()
}

/// Extracts the session id from the `Cookie` header, if present. Shared by
/// [`logout`] and [`crate::auth::check_role`].
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

    fn users_state() -> AppState {
        let mut users = HashMap::new();
        users.insert(
            "alice".to_string(),
            UserRecord {
                username: "alice".into(),
                password_hash: crate::users::hash_password("alice-pass"),
                role: Role::Admin,
            },
        );
        users.insert(
            "bob".to_string(),
            UserRecord {
                username: "bob".into(),
                password_hash: crate::users::hash_password("bob-pass"),
                role: Role::Operator,
            },
        );
        AppState::new(None).with_users(users)
    }

    async fn login_as(app: &Router, username: &str, password: &str) -> Response {
        app.clone()
            .oneshot(
                HttpRequest::post("/ui/login")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({ "username": username, "password": password })
                            .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_users_file_login_with_the_right_password_succeeds() {
        let app = router(users_state());
        let resp = login_as(&app, "alice", "alice-pass").await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn a_users_file_login_with_the_wrong_password_is_rejected() {
        let app = router(users_state());
        let resp = login_as(&app, "alice", "wrong").await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_users_file_login_with_an_unknown_username_is_rejected() {
        let app = router(users_state());
        let resp = login_as(&app, "nobody", "anything").await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn an_operator_session_carries_the_operator_role() {
        let app = router(users_state());
        let resp = login_as(&app, "bob", "bob-pass").await;
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
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["username"], "bob");
    }

    #[tokio::test]
    async fn an_operator_cannot_reach_an_admin_only_route() {
        let app = router(users_state());
        let resp = login_as(&app, "bob", "bob-pass").await; // bob is Operator
        let cookie = cookie_header_from(&set_cookie_value(&resp));

        // No --controller-url configured, but the role gate must reject
        // this before the request ever gets that far — a 503 here would
        // mean the gate was skipped, not just that there's no controller.
        let resp = app
            .oneshot(
                HttpRequest::post("/api/config")
                    .header(header::COOKIE, cookie)
                    .body(Body::from("pools: []"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_viewer_cannot_reach_an_operator_route_but_can_reach_a_viewer_one() {
        let mut users = HashMap::new();
        users.insert(
            "carol".to_string(),
            UserRecord {
                username: "carol".into(),
                password_hash: crate::users::hash_password("carol-pass"),
                role: Role::Viewer,
            },
        );
        let app = router(AppState::new(None).with_users(users));
        let resp = login_as(&app, "carol", "carol-pass").await;
        let cookie = cookie_header_from(&set_cookie_value(&resp));

        let resp = app
            .clone()
            .oneshot(
                HttpRequest::post("/api/fleet/instances/proxy-1/drain")
                    .header(header::COOKIE, cookie.clone())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // A viewer-level route: 503 (no aggregator configured), never 401/403 —
        // proves the gate let a Viewer through to the handler itself.
        let resp = app
            .oneshot(
                HttpRequest::get("/api/fleet/pools")
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
