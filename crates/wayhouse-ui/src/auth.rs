//! Session-cookie gate for everything `wayhouse-ui` serves that isn't `GET
//! /healthz` or the login endpoint itself. Distinct from
//! `wayhouse-controller`'s/`wayhouse-aggregator`'s `require_bearer`: this checks a
//! session cookie, not a bearer token — the browser never holds a bearer
//! token for anything (`docs/10` "The admin GUI").
//!
//! Phase 12 slice 8 widens the single `require_session` gate into
//! [`check_role`], parameterized by a minimum [`Role`]: missing/invalid
//! session is still `401`; a valid session whose role is too low is a new
//! `403` (distinct from `401` so the frontend can tell "not logged in" from
//! "logged in, not allowed"). On success the session's username (if any —
//! `None` in legacy `--ui-password` mode) is stashed into the request's
//! extensions as [`Actor`], for `crate::aggregator_proxy`/
//! `crate::controller_proxy` to read back out and attribute a write to
//! (`X-Actor`, see their doc comments).

use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::api::{session_id_from, AppState};
use crate::role::Role;

/// The acting username, if any — inserted into request extensions by
/// [`check_role`] on every successful gate check, read back out by the
/// proxy handlers that need to attribute a write.
#[derive(Debug, Clone, Default)]
pub struct Actor(pub Option<String>);

/// Gate requiring a session whose role is at least `min`. With no auth
/// configured at all (`state.auth_configured()` false — the default), every
/// request is treated as an anonymous `Role::Admin`, open exactly like every
/// other optional-auth surface in this fleet.
pub async fn check_role(state: AppState, mut req: Request, next: Next, min: Role) -> Response {
    if !state.auth_configured() {
        req.extensions_mut().insert(Actor(None));
        return next.run(req).await;
    }

    let Some(id) = session_id_from(&req) else {
        return unauthorized();
    };
    let Some(session) = state.sessions.get(&id) else {
        return unauthorized();
    };
    if session.role < min {
        return (StatusCode::FORBIDDEN, "forbidden").into_response();
    }

    req.extensions_mut().insert(Actor(session.username));
    next.run(req).await
}

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, "unauthorized").into_response()
}
