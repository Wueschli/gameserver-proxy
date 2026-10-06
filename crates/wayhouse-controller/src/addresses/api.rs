//! `GET /tunnel/addresses` — the address table, with a staleness flag — plus
//! the pieces both registries share: [`claim_error_response`] (the one place
//! `ClaimError` becomes an HTTP status) and the daily stale warning.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

use super::{is_stale, unix_secs, AddressBook, ClaimError, Network, Rejection};
use crate::ha::cluster_state::ClusterState;

#[derive(Clone)]
pub struct AddressesState {
    book: Arc<AddressBook>,
    /// Same posture as the registries: `None` leaves the API open.
    auth_token: Option<Arc<str>>,
    stale_after: Duration,
    /// Under HA the network is the one the cluster recorded, not the book's
    /// (the state machine hands the book the network with each entry).
    cluster: Option<Arc<ClusterState>>,
}

impl AddressesState {
    pub fn new(book: Arc<AddressBook>, auth_token: Option<String>, stale_after: Duration) -> Self {
        AddressesState {
            book,
            auth_token: auth_token.map(Arc::from),
            stale_after,
            cluster: None,
        }
    }

    /// Reports the cluster's recorded network instead of the book's own.
    pub fn with_cluster(mut self, cluster: Arc<ClusterState>) -> Self {
        self.cluster = Some(cluster);
        self
    }

    fn network(&self) -> Option<Network> {
        match &self.cluster {
            Some(cluster) => cluster.network().ok().flatten().flatten(),
            None => self.book.network(),
        }
    }
}

pub fn router(state: AddressesState) -> Router {
    let routes = Router::new()
        .route("/tunnel/addresses", get(list))
        .route_layer(axum::middleware::from_fn_with_state(
            wayhouse_http::server::BearerAuth::new(state.auth_token.as_deref()),
            wayhouse_http::server::require_bearer,
        ));
    wayhouse_http::protocol::gate(routes, "controller").with_state(state)
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

/// `409` address held / owner has a different one, `422` invalid address, no
/// network configured, backends off the claimed address or an unparsable
/// replicated network, `503` network
/// exhausted, book full or registries still initializing, `500` storage.
pub fn claim_error_response(e: &ClaimError) -> Response {
    let status = match e {
        ClaimError::Rejected(r) => match r {
            Rejection::Held { .. } | Rejection::OwnerHasDifferent { .. } => StatusCode::CONFLICT,
            Rejection::OutsideNetwork { .. }
            | Rejection::NotHost(_)
            | Rejection::NoNetwork
            | Rejection::BackendHost(_)
            | Rejection::InvalidNetwork(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Rejection::Exhausted { .. } | Rejection::Full { .. } | Rejection::NotInitialized => {
                StatusCode::SERVICE_UNAVAILABLE
            }
        },
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
    let now = unix_secs();
    let network = state.network();
    let out = ListOut {
        network: network.map(|n| n.to_string()),
        allocated: entries.len(),
        capacity: network.map(|n| n.capacity()),
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

/// The `role/name (address)` of every owner not re-registered for over
/// `stale_after`.
fn stale_owners(book: &AddressBook, stale_after: Duration, now: u64) -> Vec<String> {
    let entries = match book.entries() {
        Ok(entries) => entries,
        Err(e) => {
            tracing::warn!(error = %e, "could not read the tunnel address book to check for stale owners");
            return Vec::new();
        }
    };
    entries
        .iter()
        .filter(|e| is_stale(&e.assignment, now, stale_after))
        .map(|e| format!("{}/{} ({})", e.role, e.name, e.assignment.address))
        .collect()
}

fn stale_message(stale: &[String], stale_after: Duration) -> String {
    format!(
        "{} tunnel address(es) not re-registered for over {:?}; DELETE the registration to \
         free one: {:?}",
        stale.len(),
        stale_after,
        stale.iter().take(10).collect::<Vec<_>>()
    )
}

/// Logs one `WARN` naming up to ten stale owners; returns how many there are.
pub fn warn_stale(book: &AddressBook, stale_after: Duration, now: u64) -> usize {
    let stale = stale_owners(book, stale_after, now);
    if !stale.is_empty() {
        tracing::warn!("{}", stale_message(&stale, stale_after));
    }
    stale.len()
}

/// The stale warning, if one is due: only the HA leader warns (every replica
/// holds the same book, so one `WARN` per cluster is enough), and only when
/// some owner is stale.
pub fn stale_warning_due(
    is_leader: bool,
    book: &AddressBook,
    stale_after: Duration,
    now: u64,
) -> Option<String> {
    if !is_leader {
        return None;
    }
    let stale = stale_owners(book, stale_after, now);
    (!stale.is_empty()).then(|| stale_message(&stale, stale_after))
}

/// Logs the stale warning now and then every 24 h while `is_leader` says so
/// (always `true` without HA). Does nothing when `stale_after` is zero.
pub async fn stale_warning_loop(
    book: Arc<AddressBook>,
    stale_after: Duration,
    is_leader: impl Fn() -> bool,
) {
    if stale_after.is_zero() {
        return;
    }
    let mut tick = tokio::time::interval(Duration::from_secs(24 * 3600));
    loop {
        tick.tick().await; // the first tick is immediate
        if let Some(message) = stale_warning_due(is_leader(), &book, stale_after, unix_secs()) {
            tracing::warn!("{message}");
        }
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
        let mut req =
            HttpRequest::get("/tunnel/addresses").header(wayhouse_http::protocol::HEADER, "1.0");
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
        book.claim(Role::Origin, "fresh", None, unix_secs())
            .unwrap();
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
        let status = |r: Rejection| claim_error_response(&ClaimError::Rejected(r)).status();
        assert_eq!(
            status(Rejection::Held {
                address: a,
                role: Role::Origin,
                name: "x".into()
            }),
            StatusCode::CONFLICT
        );
        assert_eq!(
            status(Rejection::OwnerHasDifferent {
                role: Role::Origin,
                name: "x".into(),
                have: a,
                requested: a
            }),
            StatusCode::CONFLICT
        );
        assert_eq!(
            status(Rejection::OutsideNetwork {
                address: a,
                network: net
            }),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            status(Rejection::NotHost(a)),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            status(Rejection::NoNetwork),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            status(Rejection::Exhausted {
                network: net,
                allocated: 254,
                capacity: 254
            }),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            status(Rejection::Full { allocated: 3 }),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            status(Rejection::BackendHost("backend is elsewhere".into())),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            status(Rejection::InvalidNetwork("bad".into())),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            status(Rejection::NotInitialized),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            claim_error_response(&ClaimError::Storage("x".into())).status(),
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

    #[test]
    fn the_stale_warning_is_leader_only() {
        let (_s, book, _d) = state(None);
        book.claim(Role::Origin, "old", None, 1).unwrap();
        let stale_after = Duration::from_secs(86400);
        assert_eq!(stale_warning_due(false, &book, stale_after, 100_000), None);
        let due = stale_warning_due(true, &book, stale_after, 100_000).unwrap();
        assert!(due.contains("origin/old"), "{due}");
        // A leader with nothing stale has nothing to say.
        assert_eq!(
            stale_warning_due(true, &book, Duration::ZERO, 100_000),
            None
        );
    }

    #[tokio::test]
    async fn tunnel_addresses_rejects_other_major() {
        let (s, _book, _d) = state(None);
        let resp = router(s)
            .oneshot(
                HttpRequest::get("/tunnel/addresses")
                    .header(wayhouse_http::protocol::HEADER, "2.0")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UPGRADE_REQUIRED);
    }
}
