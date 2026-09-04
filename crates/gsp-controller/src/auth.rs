//! Bearer-token auth (slice 5). `gsp-controller --auth-token <token>` gates
//! every `/config*` route; omit it and the controller is open, same posture
//! `gsp`'s own admin API has today (network-boundary-only auth). `/healthz`
//! is never gated — it's wired up separately in `main.rs`, outside
//! [`crate::api::router`] — matching plain liveness-probe convention.
//!
//! This is a single shared secret, not per-caller identity or RBAC —
//! appropriate for the phase 10+11 PoC's one-controller, one-operator-token
//! scope (`docs/10` "The controller"). A comparison isn't constant-time;
//! for a token gating an internal control-plane API behind its own network
//! boundary that's an accepted trade rather than pulling in a dedicated
//! crate for it — revisit if this token ever gates something
//! internet-facing.

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
        Some(token) if token == expected => next.run(req).await,
        _ => (StatusCode::UNAUTHORIZED, "unauthorized").into_response(),
    }
}
