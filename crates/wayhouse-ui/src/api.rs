//! `POST /ui/login` / `POST /ui/logout` / `GET /ui/session` (slice 11b) —
//! the browser's own session-cookie auth. Wholly separate from every
//! machine-to-machine bearer token this fleet uses (`wayhouse-controller`'s /
//! `wayhouse-aggregator`'s `--auth-token`, `--instance-token`,
//! `--aggregator-token`, `settings.admin.auth_token`) — `wayhouse-ui` is the only
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
use std::net::IpAddr;
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::login_limit::{LoginLimiter, LoginLimits};
use crate::role::Role;
use crate::session::{Session, SessionLimits, SessionStore};
use crate::users::UserRecord;

/// The cookie the browser holds. `HttpOnly` (never readable from JS) +
/// `SameSite=Lax`, plus `Secure` when this process serves HTTPS itself
/// ([`AppState::secure_cookie`]). Behind a TLS-terminating proxy the UI sees
/// plain HTTP and leaves it off — set it at the proxy there (docs/12).
pub const SESSION_COOKIE: &str = "wayhouse_ui_session";

/// Where `crate::aggregator_proxy` sends its calls, and the bearer token it
/// presents there — a separate secret from `wayhouse-controller`'s (`docs/10`:
/// `wayhouse-ui` holds each service's own credential, never conflates them).
#[derive(Clone)]
pub struct AggregatorTarget {
    pub base_url: String,
    pub token: Option<String>,
}

/// Where `crate::controller_proxy` sends its calls, and the bearer token it
/// presents there — a separate secret from `wayhouse-aggregator`'s, same
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
    /// Mark the session cookie `Secure` — set exactly when this UI serves HTTPS
    /// itself (`--tls-cert`); on plain HTTP a browser would drop such a cookie.
    pub secure_cookie: bool,
    /// Throttles `POST /ui/login` per client address and per username.
    pub login_limiter: Arc<LoginLimiter>,
    /// Bounds concurrent Argon2 verifications, so a flood of logins can't
    /// occupy every blocking thread.
    pub verify_permits: Arc<tokio::sync::Semaphore>,
}

/// Longest username accepted at login; longer is rejected before any
/// bookkeeping.
const MAX_USERNAME_BYTES: usize = 256;

/// Concurrent Argon2 verifications allowed at once.
const MAX_CONCURRENT_VERIFIES: usize = 4;

impl AppState {
    pub fn new(ui_password: Option<String>) -> Self {
        AppState {
            ui_password: ui_password.map(Arc::from),
            users: None,
            sessions: Arc::new(SessionStore::new()),
            http: wayhouse_http::client(),
            aggregator: None,
            controller: None,
            fleet_feed: None,
            secure_cookie: false,
            login_limiter: Arc::new(LoginLimiter::default()),
            verify_permits: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_VERIFIES)),
        }
    }

    pub fn with_session_limits(mut self, limits: SessionLimits) -> Self {
        self.sessions = Arc::new(SessionStore::with_limits(limits));
        self
    }

    pub fn with_login_limits(mut self, limits: LoginLimits) -> Self {
        self.login_limiter = Arc::new(LoginLimiter::new(limits));
        self
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

    pub fn with_secure_cookie(mut self, secure: bool) -> Self {
        self.secure_cookie = secure;
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
async fn login(
    State(state): State<AppState>,
    // `ConnectInfo` as an `Extension` so a missing one (no real socket,
    // as in tests) is `None` rather than a rejection.
    peer: Option<axum::Extension<axum::extract::ConnectInfo<wayhouse_http::tls::PeerAddr>>>,
    Json(req): Json<LoginRequest>,
) -> Response {
    // No peer address (a test harness without a real socket) shares one key.
    let ip = peer.map_or(IpAddr::from([0, 0, 0, 0]), |p| (p.0).0 .0.ip());
    // Throttle before any password work. In legacy/open mode there is no
    // username, so only the per-address bucket applies.
    let username = state.users.as_ref().and(req.username.as_deref());
    // Usernames key the limiter's map, so bound their size (no real
    // username comes close).
    if username.is_some_and(|u| u.len() > MAX_USERNAME_BYTES) {
        return bad_credentials();
    }
    if let Err(wait) = state.login_limiter.check(ip, username) {
        return too_many_attempts(wait);
    }

    if let Some(users) = &state.users {
        let Some(username) = req.username.as_deref() else {
            return bad_credentials();
        };
        let user = users.get(username).cloned();
        let password = req.password;
        let Ok(_permit) = state.verify_permits.clone().acquire_owned().await else {
            return bad_credentials();
        };
        let verified = tokio::task::spawn_blocking(move || match user {
            Some(user) => crate::users::verify_password(&user.password_hash, &password)
                .then_some((user.role, user.username)),
            None => {
                // Same Argon2 cost as a known user, so timing doesn't reveal
                // which usernames exist.
                crate::users::verify_against_dummy(&password);
                None
            }
        })
        .await
        .ok()
        .flatten();
        return match verified {
            Some((role, username)) => issue_session(
                &state,
                Session {
                    role,
                    username: Some(username),
                },
            ),
            None => bad_credentials(),
        };
    }

    match state.ui_password.as_deref() {
        Some(expected) if !wayhouse_http::token_eq(&req.password, expected) => bad_credentials(),
        _ => issue_session(
            &state,
            Session {
                role: Role::Admin,
                username: None,
            },
        ),
    }
}

fn too_many_attempts(wait: std::time::Duration) -> Response {
    let secs = wait.as_secs() + 1;
    (
        StatusCode::TOO_MANY_REQUESTS,
        [(header::RETRY_AFTER, secs.to_string())],
        Json(ErrorResponse {
            error: "too many login attempts, try again later".into(),
        }),
    )
        .into_response()
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
    let cookie = format!(
        "{SESSION_COOKIE}={id}; HttpOnly; Path=/; SameSite=Lax; Max-Age={}{}",
        state.sessions.limits().max_age.as_secs(),
        secure_attr(state)
    );
    (StatusCode::OK, [(header::SET_COOKIE, cookie)], "ok").into_response()
}

fn secure_attr(state: &AppState) -> &'static str {
    if state.secure_cookie {
        "; Secure"
    } else {
        ""
    }
}

/// `POST /ui/logout` — revokes the session named by the request's cookie, if
/// any, and tells the browser to drop it. Never gated: logging out an
/// already-invalid/missing session is a no-op, not an error.
async fn logout(State(state): State<AppState>, req: Request) -> Response {
    if let Some(id) = session_id_from(&req) {
        state.sessions.revoke(&id);
    }
    let expire_cookie = format!(
        "{SESSION_COOKIE}=; HttpOnly; Path=/; SameSite=Lax; Max-Age=0{}",
        secure_attr(&state)
    );
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

/// Extracts the session id from the `Cookie` header(s), if present. Shared by
/// [`logout`] and [`crate::auth::check_role`]. Every `cookie` header counts:
/// over HTTP/2 a browser may send one per cookie (RFC 9113 §8.2.3).
pub fn session_id_from(req: &Request) -> Option<String> {
    let prefix = format!("{SESSION_COOKIE}=");
    req.headers()
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
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
        // "wayhouse_ui_session=<id>; HttpOnly; ..." -> "wayhouse_ui_session=<id>"
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

    async fn login_and_logout_cookies(state: AppState) -> (String, String) {
        let app = router(state);
        let login = app
            .clone()
            .oneshot(
                HttpRequest::post("/ui/login")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"password":"secret"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(login.status(), StatusCode::OK);
        let logout = app
            .oneshot(HttpRequest::post("/ui/logout").body(Body::empty()).unwrap())
            .await
            .unwrap();
        (set_cookie_value(&login), set_cookie_value(&logout))
    }

    fn has_attr(set_cookie: &str, attr: &str) -> bool {
        set_cookie.split(';').any(|part| part.trim() == attr)
    }

    /// Served over HTTPS (`--tls-cert`): the browser must never send the session
    /// cookie over plain HTTP, so both the cookie and its expiry carry `Secure`.
    #[tokio::test]
    async fn a_tls_ui_marks_the_session_cookie_secure() {
        let state = AppState::new(Some("secret".into())).with_secure_cookie(true);
        let (login, logout) = login_and_logout_cookies(state).await;
        assert!(has_attr(&login, "Secure"), "{login}");
        assert!(has_attr(&login, "HttpOnly"), "{login}");
        assert!(has_attr(&logout, "Secure"), "{logout}");
    }

    /// Plain HTTP: a `Secure` cookie would be dropped by the browser and login
    /// would loop, so it stays unmarked.
    #[tokio::test]
    async fn a_plain_http_ui_does_not_mark_the_cookie_secure() {
        let (login, logout) = login_and_logout_cookies(AppState::new(Some("secret".into()))).await;
        assert!(!has_attr(&login, "Secure"), "{login}");
        assert!(!has_attr(&logout, "Secure"), "{logout}");
    }

    /// HTTP/2 lets a browser send each cookie in its own `cookie` header
    /// (RFC 9113 §8.2.3; Firefox does), and hyper does not join them — the
    /// session cookie behind another site's cookie on the same host must count.
    #[tokio::test]
    async fn the_session_cookie_is_found_in_any_cookie_header() {
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

        let resp = app
            .oneshot(
                HttpRequest::get("/ui/session")
                    .header(header::COOKIE, "other_app=1")
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn a_garbage_cookie_is_rejected_not_a_panic() {
        let app = router(AppState::new(Some("secret".into())));
        let resp = app
            .oneshot(
                HttpRequest::get("/ui/session")
                    .header(header::COOKIE, "wayhouse_ui_session=not-a-real-session")
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

    async fn post_login(app: &Router, body: &'static str) -> Response {
        app.clone()
            .oneshot(
                HttpRequest::post("/ui/login")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn the_session_cookie_carries_the_absolute_max_age() {
        let app = router(AppState::new(None).with_session_limits(SessionLimits {
            max_age: std::time::Duration::from_secs(1234),
            ..SessionLimits::default()
        }));
        let resp = post_login(&app, r#"{"password":"x"}"#).await;
        assert!(set_cookie_value(&resp).contains("Max-Age=1234"));
    }

    #[tokio::test]
    async fn an_expired_session_is_rejected_like_no_session() {
        let app = router(
            AppState::new(Some("secret".into())).with_session_limits(SessionLimits {
                idle_timeout: std::time::Duration::from_millis(50),
                ..SessionLimits::default()
            }),
        );
        let resp = post_login(&app, r#"{"password":"secret"}"#).await;
        let cookie = cookie_header_from(&set_cookie_value(&resp));
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
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

    fn tight_login_limits(per_ip: u32, per_username: u32) -> LoginLimits {
        use crate::login_limit::Bucket;
        let slow = std::time::Duration::from_secs(3600);
        LoginLimits {
            per_ip: Bucket {
                burst: per_ip,
                refill: slow,
            },
            per_username: Bucket {
                burst: per_username,
                refill: slow,
            },
            max_keys: 100,
        }
    }

    #[tokio::test]
    async fn login_attempts_beyond_the_per_ip_burst_get_429_with_retry_after() {
        let app = router(
            AppState::new(Some("secret".into())).with_login_limits(tight_login_limits(2, 100)),
        );
        for _ in 0..2 {
            let resp = post_login(&app, r#"{"password":"wrong"}"#).await;
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        }
        let resp = post_login(&app, r#"{"password":"secret"}"#).await;
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(resp.headers().contains_key(header::RETRY_AFTER));
        assert!(!resp.headers().contains_key(header::SET_COOKIE));
    }

    #[tokio::test]
    async fn login_attempts_beyond_the_per_username_burst_get_429() {
        let app = router(users_state().with_login_limits(tight_login_limits(100, 1)));
        let resp = post_login(&app, r#"{"username":"alice","password":"nope"}"#).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let resp = post_login(&app, r#"{"username":"alice","password":"alice-pass"}"#).await;
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        // Another username is unaffected.
        let resp = post_login(&app, r#"{"username":"bob","password":"bob-pass"}"#).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn over_a_real_socket_the_limit_keys_on_the_peer_address() {
        let app = router(
            AppState::new(Some("secret".into())).with_login_limits(tight_login_limits(1, 100)),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<wayhouse_http::tls::PeerAddr>(),
            )
            .await
        });
        let client = reqwest::Client::new();
        let url = format!("http://{addr}/ui/login");
        let post = || {
            client
                .post(&url)
                .json(&serde_json::json!({"password": "wrong"}))
        };
        assert_eq!(post().send().await.unwrap().status(), 401);
        assert_eq!(post().send().await.unwrap().status(), 429);
    }

    #[tokio::test]
    async fn an_oversized_username_is_rejected_without_touching_the_limiter() {
        let app = router(users_state().with_login_limits(tight_login_limits(1, 1)));
        let body = format!(r#"{{"username":"{}","password":"x"}}"#, "a".repeat(5000));
        let body: &'static str = Box::leak(body.into_boxed_str());
        // Twice: a 401 both times (not 429) shows no limiter token was spent.
        for _ in 0..2 {
            let resp = post_login(&app, body).await;
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        }
    }
}
