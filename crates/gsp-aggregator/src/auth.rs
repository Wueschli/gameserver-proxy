//! Bearer-token auth (slice 10). `gsp-aggregator --auth-token <token>` gates
//! every route except `GET /healthz` — `POST /ingest`, the `/fleet/*` reads,
//! and the intent-verb fan-out writes alike. Omit it and the aggregator is
//! open, same posture as always (network-boundary-only auth). Mirrors
//! `gsp-controller`'s `auth.rs` exactly; see there for the fuller rationale
//! (single shared secret, not RBAC; comparison isn't constant-time, an
//! accepted trade for an internal control-plane API behind its own network
//! boundary).
//!
//! A proxy pushing to `POST /ingest` needs the same token
//! (`gsp --aggregator-token`); the aggregator needs its own copy of each
//! instance's admin token to fan intent verbs back out
//! (`gsp-aggregator --instance-token`) — three independent secrets for three
//! independent hops, not one token threaded through everything.

use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::api::AppState;

pub async fn require_bearer(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let Some(expected) = state.auth_token.as_deref() else {
        return next.run(req).await; // no token configured: open, like today's admin API
    };

    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    match presented {
        Some(token) if gsp_http::token_eq(token, expected) => next.run(req).await,
        _ => (StatusCode::UNAUTHORIZED, "unauthorized").into_response(),
    }
}
