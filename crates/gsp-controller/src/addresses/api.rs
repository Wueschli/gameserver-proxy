//! `GET /tunnel/addresses` — the address table, with a staleness flag — plus
//! the pieces both registries share: [`claim_error_response`] (the one place
//! `ClaimError` becomes an HTTP status) and the daily stale warning.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

use super::{is_stale, now_secs, AddressBook, ClaimError};

#[derive(Clone)]
pub struct AddressesState {
    book: Arc<AddressBook>,
    /// Same posture as the registries: `None` leaves the API open.
    auth_token: Option<Arc<str>>,
    stale_after: Duration,
}

impl AddressesState {
    pub fn new(book: Arc<AddressBook>, auth_token: Option<String>, stale_after: Duration) -> Self {
        AddressesState {
            book,
            auth_token: auth_token.map(Arc::from),
            stale_after,
        }
    }
}

pub fn router(state: AddressesState) -> Router {
    Router::new()
        .route("/tunnel/addresses", get(list))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_bearer,
        ))
        .with_state(state)
}

/// Mirrors `crate::peers::api::require_bearer`, typed against this state.
async fn require_bearer(State(state): State<AddressesState>, req: Request, next: Next) -> Response {
    let Some(expected) = state.auth_token.as_deref() else {
        return next.run(req).await;
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

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

/// `409` address held / owner has a different one, `422` invalid address or no
/// network configured, `503` network exhausted or book full, `500` storage.
pub fn claim_error_response(e: &ClaimError) -> Response {
    let status = match e {
        ClaimError::Held { .. } | ClaimError::OwnerHasDifferent { .. } => StatusCode::CONFLICT,
        ClaimError::OutsideNetwork { .. } | ClaimError::NotHost(_) | ClaimError::NoNetwork => {
            StatusCode::UNPROCESSABLE_ENTITY
        }
        ClaimError::Exhausted { .. } | ClaimError::Full { .. } => StatusCode::SERVICE_UNAVAILABLE,
        ClaimError::Storage(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (
        status,
        Json(ErrorBody {
            error: e.to_string(),
        }),
    )
        .into_response()
}

#[derive(Serialize)]
struct EntryOut {
    role: super::Role,
    name: String,
    address: String,
    first_seen: u64,
    last_seen: u64,
    stale: bool,
}

#[derive(Serialize)]
struct ListOut {
    network: Option<String>,
    allocated: usize,
    capacity: Option<u64>,
    entries: Vec<EntryOut>,
}

async fn list(State(state): State<AddressesState>) -> Response {
    let entries = match state.book.entries() {
        Ok(e) => e,
        Err(e) => return claim_error_response(&e),
    };
    let now = now_secs();
    let out = ListOut {
        network: state.book.network().map(|n| n.to_string()),
        allocated: entries.len(),
        capacity: state.book.network().map(|n| n.capacity()),
        entries: entries
            .into_iter()
            .map(|e| EntryOut {
                stale: is_stale(&e.assignment, now, state.stale_after),
                role: e.role,
                name: e.name,
                address: e.assignment.address.to_string(),
                first_seen: e.assignment.first_seen,
                last_seen: e.assignment.last_seen,
            })
            .collect(),
    };
    Json(out).into_response()
}

/// Logs one `WARN` naming up to ten stale owners; returns how many there are.
pub fn warn_stale(book: &AddressBook, stale_after: Duration, now: u64) -> usize {
    let entries = match book.entries() {
        Ok(entries) => entries,
        Err(e) => {
            tracing::warn!(error = %e, "could not read the tunnel address book to check for stale owners");
            return 0;
        }
    };
    let stale: Vec<String> = entries
        .iter()
        .filter(|e| is_stale(&e.assignment, now, stale_after))
        .map(|e| format!("{}/{} ({})", e.role, e.name, e.assignment.address))
        .collect();
    if !stale.is_empty() {
        tracing::warn!(
            count = stale.len(),
            owners = ?stale.iter().take(10).collect::<Vec<_>>(),
            "tunnel addresses not re-registered for over {:?}; DELETE the registration to free one",
            stale_after
        );
    }
    stale.len()
}

/// Logs the stale warning now and then every 24 h. Does nothing when
/// `stale_after` is zero.
pub async fn stale_warning_loop(book: Arc<AddressBook>, stale_after: Duration) {
    if stale_after.is_zero() {
        return;
    }
    let mut tick = tokio::time::interval(Duration::from_secs(24 * 3600));
    loop {
        tick.tick().await; // the first tick is immediate
        warn_stale(&book, stale_after, now_secs());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::addresses::{Network, Role};
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use tower::ServiceExt;

    fn state(token: Option<&str>) -> (AddressesState, Arc<AddressBook>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let book = Arc::new(
            AddressBook::open(dir.path(), Some(Network::parse("10.60.0.0/24").unwrap())).unwrap(),
        );
        let s = AddressesState::new(
            book.clone(),
            token.map(str::to_string),
            Duration::from_secs(86400),
        );
        (s, book, dir)
    }

    async fn get_json(app: Router, auth: Option<&str>) -> (StatusCode, serde_json::Value) {
        let mut req = HttpRequest::get("/tunnel/addresses");
        if let Some(a) = auth {
            req = req.header("authorization", a);
        }
        let resp = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    #[tokio::test]
    async fn the_table_lists_owners_with_counts_and_the_stale_flag() {
        let (s, book, _d) = state(None);
        // One fresh, one last seen long ago.
        book.claim(Role::Origin, "fresh", None, now_secs()).unwrap();
        book.claim(Role::Proxy, "old", None, 1).unwrap();
        let (status, body) = get_json(router(s), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["network"], "10.60.0.0/24");
        assert_eq!(body["allocated"], 2);
        assert_eq!(body["capacity"], 254);
        let entries = body["entries"].as_array().unwrap();
        let find = |n: &str| entries.iter().find(|e| e["name"] == n).unwrap();
        assert_eq!(find("fresh")["stale"], false);
        assert_eq!(find("old")["stale"], true);
        assert_eq!(find("old")["role"], "proxy");
        assert_eq!(find("fresh")["address"], "10.60.0.1");
    }

    #[tokio::test]
    async fn the_table_requires_the_bearer_token_when_one_is_configured() {
        let (s, _book, _d) = state(Some("secret"));
        let (status, _) = get_json(router(s.clone()), None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) = get_json(router(s), Some("Bearer secret")).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[test]
    fn claim_errors_map_to_the_documented_statuses() {
        use std::net::IpAddr;
        let a: IpAddr = "10.60.0.1".parse().unwrap();
        let net = Network::parse("10.60.0.0/24").unwrap();
        let status = |e: ClaimError| claim_error_response(&e).status();
        assert_eq!(
            status(ClaimError::Held {
                address: a,
                role: Role::Origin,
                name: "x".into()
            }),
            StatusCode::CONFLICT
        );
        assert_eq!(
            status(ClaimError::OwnerHasDifferent {
                role: Role::Origin,
                name: "x".into(),
                have: a,
                requested: a
            }),
            StatusCode::CONFLICT
        );
        assert_eq!(
            status(ClaimError::OutsideNetwork {
                address: a,
                network: net
            }),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            status(ClaimError::NotHost(a)),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            status(ClaimError::NoNetwork),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            status(ClaimError::Exhausted {
                network: net,
                allocated: 254,
                capacity: 254
            }),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            status(ClaimError::Storage("x".into())),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn warn_stale_counts_only_stale_owners() {
        let (_s, book, _d) = state(None);
        book.claim(Role::Origin, "fresh", None, 100_000).unwrap();
        book.claim(Role::Origin, "old", None, 1).unwrap();
        assert_eq!(warn_stale(&book, Duration::from_secs(86400), 100_000), 1);
        assert_eq!(warn_stale(&book, Duration::ZERO, 100_000), 0);
    }
}
