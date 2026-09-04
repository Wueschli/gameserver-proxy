//! Session-cookie gate (slice 11b) for everything `gsp-ui` serves that isn't
//! `GET /healthz` or the login endpoint itself. Distinct from
//! `gsp-controller`'s/`gsp-aggregator`'s `require_bearer`: this checks a
//! session cookie, not a bearer token — the browser never holds a bearer
//! token for anything (`docs/10` "The admin GUI").

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::api::{session_id_from, AppState};

pub async fn require_session(State(state): State<AppState>, req: Request, next: Next) -> Response {
    if state.ui_password.is_none() {
        return next.run(req).await; // no password configured: open, like every other optional-auth surface in this fleet
    }

    match session_id_from(&req) {
        Some(id) if state.sessions.is_valid(&id) => next.run(req).await,
        _ => (StatusCode::UNAUTHORIZED, "unauthorized").into_response(),
    }
}
