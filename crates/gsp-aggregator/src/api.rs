//! `POST /ingest` (slice 6) — a proxy pushes its own [`IngestPayload`]
//! periodically; the aggregator stores latest-write-wins per `instance`.
//!
//! `GET /fleet/pools` / `/fleet/sessions` / `/fleet/healthz` (slice 8) serve
//! a merged fleet view straight off the same [`IngestStore`] — no fan-out
//! call to any instance, the data already arrived. Each entry carries
//! `last_seen_ms_ago` (the aggregator's own clock, not the proxy's — see
//! [`crate::ingest`]'s note on why); `/fleet/healthz` additionally flags an
//! instance `stale` past [`STALE_AFTER_MS`], which is where "stale" first
//! gets an actual threshold (`IngestStore` itself has none).
//!
//! **No `GET /fleet/config`** in this release: `IngestPayload` carries
//! structural/health *state*, not config *content* — the controller (docs/10
//! "The controller") is the one authoritative source for config, via its own
//! `GET /config`/`/config/revisions`. A fleet-wide "which revision is each
//! instance actually running" view is a real, useful, and still-open
//! addition (an optional `config_revision` field on `IngestPayload`,
//! self-reported by `gsp` when in `--controller` mode) — deferred because it
//! needs `controller_client` and `aggregator_client` to share state inside
//! `gsp` that today are deliberately independent ("unrelated axes", slice
//! 7's doc comment), not because it doesn't matter.
//!
//! `GET /fleet/subscribe` (slice 11a) is the machine-to-machine feed
//! `gsp-ui` subscribes to for live updates: current merged fleet state (the
//! same shape as combining `/fleet/pools` + `/fleet/sessions` + a `stale`
//! flag) on connect, then again on every accepted `POST /ingest` — debounced
//! against a burst of pushes landing close together. Reuses the exact
//! catch-up-then-broadcast SSE shape `gsp-controller`'s `/config/subscribe`
//! already proved out, applied to *state* instead of a revision log, so
//! there's no cursor/replay semantics to get right: a lagged subscriber just
//! gets the *current* view on its next tick, same as a fresh connect would.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::{Stream, StreamExt};

use crate::ingest::{IngestPayload, IngestStore, PoolSummary, SessionCounts};
use crate::util::now_ms;

/// How long since an instance's last push before `/fleet/healthz` calls it
/// `stale` — roughly 3x `gsp`'s default `--aggregator-interval-sec` (10s),
/// generous enough to absorb one missed tick without false-flagging a
/// perfectly healthy instance.
const STALE_AFTER_MS: u64 = 30_000;

/// How long `/fleet/subscribe` waits after a signal before rebuilding and
/// sending the fleet view, draining any further signals that land in that
/// window — coalesces a burst of near-simultaneous pushes (a whole fleet's
/// push interval lining up) into one rebuild instead of one per instance.
const SUBSCRIBE_DEBOUNCE: Duration = Duration::from_millis(150);

/// Capacity of the ingest-update broadcast — generous for a control plane
/// (pushes are seconds apart, not a hot path).
const UPDATES_CAPACITY: usize = 64;

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<IngestStore>,
    /// Shared client for `crate::fanout`'s calls out to each instance's own
    /// admin API. `reqwest::Client` is cheap to clone (an `Arc` internally)
    /// and reuses connections, so one lives on `AppState` rather than being
    /// built per call.
    pub http: reqwest::Client,
    /// Bearer token every request except `GET /healthz` must present
    /// (`--auth-token`); `None` leaves this aggregator's own API open.
    pub auth_token: Option<String>,
    /// Bearer token `crate::fanout` presents to every instance's admin API
    /// (`--instance-token`) — a separate secret from `auth_token`: one gates
    /// calls *into* this aggregator, the other is what this aggregator
    /// presents *out* to instances that require `settings.admin.auth_token`.
    pub instance_token: Option<String>,
    /// Fires (with no payload — subscribers rebuild the view themselves from
    /// `store`) on every accepted `POST /ingest`, for `/fleet/subscribe`.
    pub updates: broadcast::Sender<()>,
}

impl AppState {
    pub fn new(store: Arc<IngestStore>) -> Self {
        let (updates, _rx) = broadcast::channel(UPDATES_CAPACITY);
        AppState {
            store,
            http: reqwest::Client::new(),
            auth_token: None,
            instance_token: None,
            updates,
        }
    }

    pub fn with_auth_token(mut self, auth_token: Option<String>) -> Self {
        self.auth_token = auth_token;
        self
    }

    pub fn with_instance_token(mut self, instance_token: Option<String>) -> Self {
        self.instance_token = instance_token;
        self
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/ingest", post(ingest))
        .route("/fleet/pools", get(fleet_pools))
        .route("/fleet/sessions", get(fleet_sessions))
        .route("/fleet/healthz", get(fleet_healthz))
        .route("/fleet/subscribe", get(subscribe_fleet))
        .merge(crate::fanout::router())
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::auth::require_bearer,
        ))
        .with_state(state)
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

/// `POST /ingest` — body is one [`IngestPayload`] as JSON. `instance` must
/// be non-empty (it's the key everything is stored and overwritten under);
/// anything else in the payload is accepted as-is, this is a summary a proxy
/// self-reports, not something the aggregator validates against reality.
async fn ingest(State(state): State<AppState>, Json(payload): Json<IngestPayload>) -> Response {
    if payload.instance.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "instance must not be empty".into(),
            }),
        )
            .into_response();
    }
    let instance = payload.instance.clone();
    state.store.ingest(payload);
    // No subscribers connected right now is not an error — the state is
    // durably held in `store` regardless; a subscriber that connects later
    // just gets it in its initial view.
    let _ = state.updates.send(());
    tracing::debug!(instance, "ingested a push from a proxy instance");
    StatusCode::OK.into_response()
}

#[derive(Serialize)]
struct FleetPools {
    instance: String,
    last_seen_ms_ago: u64,
    pools: Vec<PoolSummary>,
}

/// `GET /fleet/pools` — every known instance's pool/backend summary, oldest-
/// pushed-first is not the order here; [`IngestStore::snapshot`] already
/// sorts by instance name for a stable table.
async fn fleet_pools(State(state): State<AppState>) -> Response {
    let now = now_ms();
    let out: Vec<FleetPools> = state
        .store
        .snapshot()
        .into_iter()
        .map(|s| FleetPools {
            instance: s.payload.instance,
            last_seen_ms_ago: now.saturating_sub(s.received_at_ms),
            pools: s.payload.pools,
        })
        .collect();
    (StatusCode::OK, Json(out)).into_response()
}

#[derive(Serialize)]
struct FleetSessions {
    instance: String,
    last_seen_ms_ago: u64,
    tcp: usize,
    udp: usize,
}

/// `GET /fleet/sessions` — every known instance's session *counts* (not the
/// full live registry — see the module doc's note on `IngestPayload` being a
/// summary; an instance's own `GET /sessions` still has the detail).
async fn fleet_sessions(State(state): State<AppState>) -> Response {
    let now = now_ms();
    let out: Vec<FleetSessions> = state
        .store
        .snapshot()
        .into_iter()
        .map(|s| FleetSessions {
            instance: s.payload.instance,
            last_seen_ms_ago: now.saturating_sub(s.received_at_ms),
            tcp: s.payload.sessions.tcp,
            udp: s.payload.sessions.udp,
        })
        .collect();
    (StatusCode::OK, Json(out)).into_response()
}

#[derive(Serialize)]
struct FleetHealth {
    instance: String,
    last_seen_ms_ago: u64,
    /// Past [`STALE_AFTER_MS`] since the last push — advisory, the same
    /// spirit as a proxy's own backend health: an operator signal, not a
    /// promise the instance is actually down (it could just be a slow
    /// aggregator link).
    stale: bool,
}

/// `GET /fleet/healthz` — every known instance's push recency. This is
/// distinct from the aggregator's own `GET /healthz` (its own liveness) and
/// from any one instance's `GET /healthz` (that instance's own liveness) —
/// it answers "when did *we* last hear from each instance."
async fn fleet_healthz(State(state): State<AppState>) -> Response {
    let now = now_ms();
    let out: Vec<FleetHealth> = state
        .store
        .snapshot()
        .into_iter()
        .map(|s| {
            let age = now.saturating_sub(s.received_at_ms);
            FleetHealth {
                instance: s.payload.instance,
                last_seen_ms_ago: age,
                stale: age > STALE_AFTER_MS,
            }
        })
        .collect();
    (StatusCode::OK, Json(out)).into_response()
}

#[derive(Serialize)]
struct FleetInstanceView {
    instance: String,
    last_seen_ms_ago: u64,
    stale: bool,
    pools: Vec<PoolSummary>,
    sessions: SessionCounts,
}

/// The merged view `/fleet/subscribe` sends — everything the three plain
/// `GET /fleet/*` endpoints report, combined into one payload so `gsp-ui`
/// doesn't need three separate subscriptions.
fn fleet_view(store: &IngestStore) -> Vec<FleetInstanceView> {
    let now = now_ms();
    store
        .snapshot()
        .into_iter()
        .map(|s| {
            let age = now.saturating_sub(s.received_at_ms);
            FleetInstanceView {
                instance: s.payload.instance,
                last_seen_ms_ago: age,
                stale: age > STALE_AFTER_MS,
                pools: s.payload.pools,
                sessions: s.payload.sessions,
            }
        })
        .collect()
}

/// `GET /fleet/subscribe` — see the module doc. Spawns
/// [`subscribe_fleet_worker`] to do the actual send-then-tail work and turns
/// its output into SSE `Event`s, keeping the worker's logic (the part worth
/// testing) free of any HTTP/SSE framing — same split `gsp-controller`'s
/// `subscribe`/`subscribe_worker` uses.
async fn subscribe_fleet(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let (tx, rx) = mpsc::channel(4);
    let updates = state.updates.subscribe();
    tokio::spawn(subscribe_fleet_worker(state.store, updates, tx));

    let events = ReceiverStream::new(rx)
        .map(|view| Ok(Event::default().data(serde_json::to_string(&view).unwrap_or_default())));
    Sse::new(events).keep_alive(KeepAlive::default())
}

/// Sends the current fleet view immediately, then again every time `updates`
/// fires — debounced by [`SUBSCRIBE_DEBOUNCE`] so a burst of near-
/// simultaneous pushes across a fleet collapses into one rebuild. A
/// [`broadcast::error::RecvError::Lagged`] just means "rebuild now" too:
/// there's no history to replay, only ever a current view, so falling behind
/// the notification channel loses nothing a fresh rebuild doesn't already
/// fix.
async fn subscribe_fleet_worker(
    store: Arc<IngestStore>,
    mut updates: broadcast::Receiver<()>,
    tx: mpsc::Sender<Vec<FleetInstanceView>>,
) {
    if tx.send(fleet_view(&store)).await.is_err() {
        return;
    }

    loop {
        match updates.recv().await {
            Ok(()) | Err(broadcast::error::RecvError::Lagged(_)) => {
                tokio::time::sleep(SUBSCRIBE_DEBOUNCE).await;
                // Drain anything else that landed during the debounce
                // window so it doesn't trigger a second, redundant rebuild
                // right behind this one.
                while updates.try_recv().is_ok() {}
                if tx.send(fleet_view(&store)).await.is_err() {
                    return;
                }
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::InstanceState;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn body_json(resp: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn test_state() -> AppState {
        AppState::new(Arc::new(IngestStore::new()))
    }

    fn payload_json(instance: &str) -> String {
        serde_json::to_string(&IngestPayload {
            instance: instance.to_string(),
            admin_url: "http://127.0.0.1:0".to_string(),
            pools: vec![],
            sessions: SessionCounts { tcp: 3, udp: 7 },
        })
        .unwrap()
    }

    #[tokio::test]
    async fn a_valid_push_is_accepted_and_stored() {
        let state = test_state();
        let store = state.store.clone();
        let app = router(state);

        let resp = app
            .oneshot(
                Request::post("/ingest")
                    .header("content-type", "application/json")
                    .body(Body::from(payload_json("proxy-1")))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let stored = store.get("proxy-1").unwrap();
        assert_eq!(stored.payload.sessions.tcp, 3);
        assert_eq!(stored.payload.sessions.udp, 7);
    }

    #[tokio::test]
    async fn an_empty_instance_name_is_rejected() {
        let app = router(test_state());
        let resp = app
            .oneshot(
                Request::post("/ingest")
                    .header("content-type", "application/json")
                    .body(Body::from(payload_json("  ")))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn malformed_json_is_rejected_by_the_extractor() {
        let app = router(test_state());
        let resp = app
            .oneshot(
                Request::post("/ingest")
                    .header("content-type", "application/json")
                    .body(Body::from("not json"))
                    .unwrap(),
            )
            .await
            .unwrap();
        // axum's `Json` extractor rejects a body that doesn't even parse as
        // JSON with 400 (a 422 would be a body that parses but fails
        // `IngestPayload`'s shape) — this never reaches our own handler.
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn a_second_push_from_the_same_instance_overwrites_the_first() {
        let state = test_state();
        let store = state.store.clone();
        let app = router(state);

        app.clone()
            .oneshot(
                Request::post("/ingest")
                    .header("content-type", "application/json")
                    .body(Body::from(payload_json("proxy-1")))
                    .unwrap(),
            )
            .await
            .unwrap();

        let mut second = serde_json::from_str::<IngestPayload>(&payload_json("proxy-1")).unwrap();
        second.sessions = SessionCounts { tcp: 99, udp: 0 };
        app.oneshot(
            Request::post("/ingest")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&second).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

        assert_eq!(store.snapshot().len(), 1);
        assert_eq!(store.get("proxy-1").unwrap().payload.sessions.tcp, 99);
    }

    fn full_payload(instance: &str) -> IngestPayload {
        IngestPayload {
            instance: instance.to_string(),
            admin_url: "http://127.0.0.1:0".to_string(),
            pools: vec![PoolSummary {
                name: "local".into(),
                balancer: "round_robin".into(),
                backends: vec![crate::ingest::BackendSummary {
                    addr: "127.0.0.1:9001".into(),
                    healthy: true,
                    state: "enabled".into(),
                    active: 3,
                }],
            }],
            sessions: SessionCounts { tcp: 5, udp: 10 },
        }
    }

    #[tokio::test]
    async fn fleet_pools_reports_every_instances_pool_and_backend_state() {
        let state = test_state();
        state.store.ingest(full_payload("proxy-1"));
        let app = router(state);

        let resp = app
            .oneshot(Request::get("/fleet/pools").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let list = body_json(resp).await;
        assert_eq!(list[0]["instance"], "proxy-1");
        assert_eq!(list[0]["pools"][0]["name"], "local");
        assert_eq!(list[0]["pools"][0]["backends"][0]["addr"], "127.0.0.1:9001");
        assert!(list[0]["last_seen_ms_ago"].as_u64().is_some());
    }

    #[tokio::test]
    async fn fleet_sessions_reports_every_instances_counts() {
        let state = test_state();
        state.store.ingest(full_payload("proxy-1"));
        let app = router(state);

        let resp = app
            .oneshot(Request::get("/fleet/sessions").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let list = body_json(resp).await;
        assert_eq!(list[0]["instance"], "proxy-1");
        assert_eq!(list[0]["tcp"], 5);
        assert_eq!(list[0]["udp"], 10);
    }

    #[tokio::test]
    async fn fleet_pools_and_sessions_are_empty_lists_before_any_push() {
        let app = router(test_state());
        for path in ["/fleet/pools", "/fleet/sessions", "/fleet/healthz"] {
            let resp = app
                .clone()
                .oneshot(Request::get(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            assert_eq!(body_json(resp).await, serde_json::json!([]));
        }
    }

    #[tokio::test]
    async fn fleet_healthz_flags_a_stale_instance_but_not_a_fresh_one() {
        let state = test_state();
        state.store.insert_state(InstanceState {
            payload: full_payload("stale-1"),
            received_at_ms: now_ms().saturating_sub(STALE_AFTER_MS + 5_000),
        });
        state.store.insert_state(InstanceState {
            payload: full_payload("fresh-1"),
            received_at_ms: now_ms(),
        });
        let app = router(state);

        let resp = app
            .oneshot(Request::get("/fleet/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let list = body_json(resp).await;
        let by_instance = |name: &str| {
            list.as_array()
                .unwrap()
                .iter()
                .find(|v| v["instance"] == name)
                .unwrap()
                .clone()
        };
        assert_eq!(by_instance("stale-1")["stale"], true);
        assert_eq!(by_instance("fresh-1")["stale"], false);
    }

    #[tokio::test]
    async fn auth_token_gates_ingest_and_fleet_reads_but_not_healthz() {
        // /healthz lives outside `api::router` in `main.rs`, so it's not
        // part of what's under test here — only confirming everything
        // *inside* this router is gated when a token is set.
        let state =
            AppState::new(Arc::new(IngestStore::new())).with_auth_token(Some("secret".into()));
        let app = router(state);

        let resp = app
            .clone()
            .oneshot(Request::get("/fleet/pools").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        let resp = app
            .clone()
            .oneshot(
                Request::get("/fleet/pools")
                    .header("Authorization", "Bearer wrong")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        let resp = app
            .oneshot(
                Request::get("/fleet/pools")
                    .header("Authorization", "Bearer secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn subscribe_worker_sends_the_current_view_immediately() {
        let store = Arc::new(IngestStore::new());
        store.ingest(full_payload("proxy-1"));

        let (updates_tx, updates_rx) = broadcast::channel(8);
        let (tx, mut rx) = mpsc::channel(8);
        tokio::spawn(subscribe_fleet_worker(store, updates_rx, tx));

        let view = rx.recv().await.unwrap();
        assert_eq!(view.len(), 1);
        assert_eq!(view[0].instance, "proxy-1");
        assert_eq!(view[0].sessions.tcp, 5);
        let _ = updates_tx; // keep the sender alive for the worker's lifetime
    }

    #[tokio::test]
    async fn subscribe_worker_resends_after_an_update_once_debounced() {
        let store = Arc::new(IngestStore::new());
        store.ingest(full_payload("proxy-1"));

        let (updates_tx, updates_rx) = broadcast::channel(8);
        let (tx, mut rx) = mpsc::channel(8);
        tokio::spawn(subscribe_fleet_worker(store.clone(), updates_rx, tx));

        // Initial view.
        assert_eq!(rx.recv().await.unwrap().len(), 1);

        // A second instance pushes; the worker must pick it up after
        // debouncing, without the test needing to know the exact delay.
        store.ingest(full_payload("proxy-2"));
        updates_tx.send(()).unwrap();

        let view = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("worker should resend well within the debounce + margin")
            .unwrap();
        assert_eq!(view.len(), 2);
    }

    #[tokio::test]
    async fn a_burst_of_updates_collapses_into_one_resend() {
        let store = Arc::new(IngestStore::new());
        store.ingest(full_payload("proxy-1"));

        let (updates_tx, updates_rx) = broadcast::channel(8);
        let (tx, mut rx) = mpsc::channel(8);
        tokio::spawn(subscribe_fleet_worker(store.clone(), updates_rx, tx));
        assert_eq!(rx.recv().await.unwrap().len(), 1); // initial view

        // Five near-simultaneous pushes/signals — the debounce should
        // collapse these into exactly one resend, not five.
        for i in 0..5 {
            store.ingest(full_payload(&format!("proxy-burst-{i}")));
            updates_tx.send(()).unwrap();
        }

        let view = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(view.len(), 6, "one instance plus the five burst instances");

        // Nothing else should arrive — the burst was one rebuild, not five.
        let extra = tokio::time::timeout(std::time::Duration::from_millis(300), rx.recv()).await;
        assert!(
            extra.is_err(),
            "no second resend should follow the debounced one"
        );
    }
}
