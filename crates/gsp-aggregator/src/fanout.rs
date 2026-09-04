//! Intent-verb fan-out (slice 9): proxies the phase-5 admin verbs to one or
//! all known instances' own admin APIs. Every known instance's `admin_url`
//! ([`crate::ingest::IngestPayload`]) is the only "backend registry" this
//! needs — no separate discovery mechanism, the aggregator just relays to
//! whatever URL an instance told it to use in its last push.
//!
//! Every handler here is a thin, stateless proxy: the aggregator decides
//! nothing and stores no intent of its own (`docs/10` "The aggregator"
//! carries no authority). Two shapes:
//!
//! - **Targeted** (`/fleet/instances/{instance}/...`): exactly one named
//!   instance. Draining *an* instance only ever means one instance — there's
//!   no sensible "drain the whole fleet" reading of that verb. The response
//!   is a pass-through of that instance's own status + body, or `404` if the
//!   name isn't a known instance, or `502` if the instance couldn't be
//!   reached.
//! - **Broadcast** (`/fleet/pools/...`, `/fleet/route-hint`): every known
//!   instance at once — a backend add/remove or a route-hint has no single
//!   shared owner across instances in this release (operator intent stays
//!   per-instance, `docs/08` phase 10+11's locked scope), so applying it
//!   fleet-wide means calling every instance's own admin API independently.
//!   The response reports **per-instance results**, never fails outright
//!   for one bad instance (`docs/10`, "The aggregator").
//!
//! Every forwarded body gets `Content-Type: application/json` set
//! explicitly, regardless of what (if anything) the original caller of this
//! aggregator sent — found live: a caller that omits it gets a `415` from
//! the target instance's own `Json` extractor, and these endpoints are
//! always JSON per the phase-5 admin API contract, so there's no reason to
//! make every caller remember a header the aggregator already knows the
//! answer to.

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{patch, post};
use axum::{Json, Router};
use serde::Serialize;

use crate::api::AppState;

/// Route definitions only — no `.with_state()` here. `api::router` merges
/// this (still generic over `AppState`) into its own routes and calls
/// `.with_state()` exactly once, at the end, for the combined router; axum
/// requires both halves of a `merge()` to share the same not-yet-substituted
/// state type; a builder that has already called `.with_state()` doesn't
/// merge back into one that hasn't.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/fleet/instances/{instance}/drain", post(drain_instance))
        .route(
            "/fleet/instances/{instance}/undrain",
            post(undrain_instance),
        )
        .route("/fleet/pools/{pool}/backends", post(add_backend))
        .route(
            "/fleet/pools/{pool}/backends/{addr}",
            patch(patch_backend).delete(delete_backend),
        )
        .route("/fleet/route-hint", post(route_hint))
}

async fn drain_instance(State(state): State<AppState>, Path(instance): Path<String>) -> Response {
    proxy_to_instance(&state, &instance, Method::POST, "/admin/drain", None).await
}

async fn undrain_instance(State(state): State<AppState>, Path(instance): Path<String>) -> Response {
    proxy_to_instance(&state, &instance, Method::POST, "/admin/undrain", None).await
}

async fn add_backend(
    State(state): State<AppState>,
    Path(pool): Path<String>,
    body: Bytes,
) -> Response {
    broadcast(
        &state,
        Method::POST,
        &format!("/pools/{pool}/backends"),
        Some(body),
    )
    .await
}

async fn patch_backend(
    State(state): State<AppState>,
    Path((pool, addr)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    broadcast(
        &state,
        Method::PATCH,
        &format!("/pools/{pool}/backends/{addr}"),
        Some(body),
    )
    .await
}

async fn delete_backend(
    State(state): State<AppState>,
    Path((pool, addr)): Path<(String, String)>,
) -> Response {
    broadcast(
        &state,
        Method::DELETE,
        &format!("/pools/{pool}/backends/{addr}"),
        None,
    )
    .await
}

async fn route_hint(State(state): State<AppState>, body: Bytes) -> Response {
    broadcast(&state, Method::POST, "/route-hint", Some(body)).await
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

/// Proxies one call to exactly one named instance, passing its response
/// (status + body) straight through unchanged.
async fn proxy_to_instance(
    state: &AppState,
    instance: &str,
    method: Method,
    path_suffix: &str,
    body: Option<Bytes>,
) -> Response {
    let Some(inst) = state.store.get(instance) else {
        return (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("unknown instance {instance}"),
            }),
        )
            .into_response();
    };

    let url = format!("{}{}", inst.payload.admin_url, path_suffix);
    let mut req = state.http.request(method, &url);
    if let Some(body) = body {
        req = req.header("content-type", "application/json").body(body);
    }

    match req.send().await {
        Ok(resp) => {
            let status = resp.status();
            let body = resp.bytes().await.unwrap_or_default();
            (status, body).into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(ErrorResponse {
                error: format!("{instance} ({url}): {e}"),
            }),
        )
            .into_response(),
    }
}

#[derive(Serialize)]
struct InstanceResult {
    instance: String,
    /// The instance's own HTTP status, if it was reached at all.
    status: Option<u16>,
    /// Set instead of `status` when the instance couldn't be reached.
    error: Option<String>,
}

/// Calls every known instance's admin API at once, concurrently (a
/// `tokio::task::JoinSet` — already-available tokio machinery, no need for a
/// `futures`-crate `join_all` dependency for this), and reports each one's
/// outcome individually — a bad or unreachable instance never fails the call
/// for the rest of the fleet.
async fn broadcast(
    state: &AppState,
    method: Method,
    path_suffix: &str,
    body: Option<Bytes>,
) -> Response {
    let instances = state.store.snapshot();
    let mut calls = tokio::task::JoinSet::new();
    for inst in instances {
        let client = state.http.clone();
        let method = method.clone();
        let url = format!("{}{}", inst.payload.admin_url, path_suffix);
        let body = body.clone();
        let instance = inst.payload.instance;
        calls.spawn(async move {
            let mut req = client.request(method, &url);
            if let Some(body) = body {
                req = req.header("content-type", "application/json").body(body);
            }
            match req.send().await {
                Ok(resp) => InstanceResult {
                    instance,
                    status: Some(resp.status().as_u16()),
                    error: None,
                },
                Err(e) => InstanceResult {
                    instance,
                    status: None,
                    error: Some(e.to_string()),
                },
            }
        });
    }

    let mut results = Vec::new();
    while let Some(joined) = calls.join_next().await {
        // A task panicking would be a bug in the closure above, not a
        // per-instance failure worth reporting in the response shape — if
        // it ever happens, it's better surfaced as a server error via the
        // panic propagating in tests than silently swallowed here.
        results.push(joined.expect("fan-out task panicked"));
    }
    // `join_next` yields completion order, not request order; sort for a
    // stable, diffable response.
    results.sort_by(|a, b| a.instance.cmp(&b.instance));

    (
        StatusCode::OK,
        Json(serde_json::json!({ "results": results })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::{IngestPayload, IngestStore, PoolSummary, SessionCounts};
    use axum::body::Body;
    use axum::http::Request;
    use std::sync::{Arc, Mutex};
    use tower::ServiceExt;

    fn ingest_payload(instance: &str, admin_url: &str) -> IngestPayload {
        IngestPayload {
            instance: instance.to_string(),
            admin_url: admin_url.to_string(),
            pools: vec![PoolSummary {
                name: "local".into(),
                balancer: "round_robin".into(),
                backends: vec![],
            }],
            sessions: SessionCounts::default(),
        }
    }

    fn test_state() -> AppState {
        AppState::new(Arc::new(IngestStore::new()))
    }

    /// A minimal stand-in for a proxy's own admin API: one route, a fixed
    /// response, and the last request body it received (so a test can
    /// confirm a broadcast/proxy call carried the right payload through).
    async fn spawn_mock_instance(
        path: &'static str,
        method: Method,
        status: StatusCode,
        response_body: &'static str,
    ) -> (String, Arc<Mutex<Option<Vec<u8>>>>) {
        let captured: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
        let captured_for_handler = captured.clone();
        let handler = move |body: Bytes| {
            let captured = captured_for_handler.clone();
            async move {
                *captured.lock().unwrap() = Some(body.to_vec());
                (status, response_body)
            }
        };
        let method_router = match method {
            Method::POST => axum::routing::post(handler),
            Method::PATCH => axum::routing::patch(handler),
            Method::DELETE => axum::routing::delete(handler),
            _ => unreachable!("test helper only used with POST/PATCH/DELETE"),
        };
        let app = Router::new().route(path, method_router);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), captured)
    }

    #[tokio::test]
    async fn drain_proxies_to_the_named_instance_and_passes_the_response_through() {
        let (base_url, _captured) =
            spawn_mock_instance("/admin/drain", Method::POST, StatusCode::OK, "draining\n").await;
        let state = test_state();
        state.store.ingest(ingest_payload("proxy-1", &base_url));
        let app = crate::api::router(state);

        let resp = app
            .oneshot(
                Request::post("/fleet/instances/proxy-1/drain")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(bytes, "draining\n".as_bytes());
    }

    #[tokio::test]
    async fn drain_an_unknown_instance_is_404() {
        let app = crate::api::router(test_state());
        let resp = app
            .oneshot(
                Request::post("/fleet/instances/nope/drain")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn add_backend_broadcasts_to_every_instance_and_forwards_the_body() {
        let (url1, captured1) = spawn_mock_instance(
            "/pools/local/backends",
            Method::POST,
            StatusCode::OK,
            "added\n",
        )
        .await;
        let (url2, captured2) = spawn_mock_instance(
            "/pools/local/backends",
            Method::POST,
            StatusCode::BAD_REQUEST,
            "unknown pool\n",
        )
        .await;
        let state = test_state();
        state.store.ingest(ingest_payload("a", &url1));
        state.store.ingest(ingest_payload("b", &url2));
        let app = crate::api::router(state);

        let resp = app
            .oneshot(
                Request::post("/fleet/pools/local/backends")
                    .body(Body::from(r#"{"addr":"127.0.0.1:9001"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let results = body["results"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        let a = results.iter().find(|r| r["instance"] == "a").unwrap();
        assert_eq!(a["status"], 200);
        let b = results.iter().find(|r| r["instance"] == "b").unwrap();
        assert_eq!(b["status"], 400);

        assert_eq!(
            captured1.lock().unwrap().as_deref(),
            Some(r#"{"addr":"127.0.0.1:9001"}"#.as_bytes())
        );
        assert_eq!(
            captured2.lock().unwrap().as_deref(),
            Some(r#"{"addr":"127.0.0.1:9001"}"#.as_bytes())
        );
    }

    /// Regression test for a bug caught live: a caller that posts to the
    /// aggregator without a `Content-Type` header used to forward that
    /// missing header straight through, and a real proxy's `Json<T>`
    /// extractor (like `gsp`'s own admin API) rejects that with `415` —
    /// even though the body is perfectly valid JSON. The mock instance here
    /// uses a real `Json` extractor (not the raw-`Bytes` one the other tests
    /// use) specifically to exercise that content-negotiation path.
    #[tokio::test]
    async fn a_broadcast_sets_content_type_even_if_the_caller_never_did() {
        async fn json_only_handler(
            axum::extract::Json(_body): axum::extract::Json<serde_json::Value>,
        ) -> &'static str {
            "added\n"
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route("/pools/local/backends", post(json_only_handler));
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let state = test_state();
        state
            .store
            .ingest(ingest_payload("a", &format!("http://{addr}")));
        let app = crate::api::router(state);

        // No content-type header on the incoming request at all — the
        // exact shape of the live repro (`curl -d '...'` without `-H
        // content-type: application/json`).
        let resp = app
            .oneshot(
                Request::post("/fleet/pools/local/backends")
                    .body(Body::from(r#"{"addr":"127.0.0.1:9001"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            body["results"][0]["status"], 200,
            "the target's Json extractor must have accepted the forwarded body, \
             which only happens if the aggregator set content-type itself"
        );
    }

    #[tokio::test]
    async fn a_broadcast_reports_an_unreachable_instance_without_failing_the_others() {
        let (url_ok, _) =
            spawn_mock_instance("/route-hint", Method::POST, StatusCode::OK, "ok\n").await;
        // An address nothing listens on — guaranteed connection failure,
        // no mock server needed.
        let unreachable_url = "http://127.0.0.1:1".to_string();

        let state = test_state();
        state.store.ingest(ingest_payload("reachable", &url_ok));
        state
            .store
            .ingest(ingest_payload("unreachable", &unreachable_url));
        let app = crate::api::router(state);

        let resp = app
            .oneshot(
                Request::post("/fleet/route-hint")
                    .body(Body::from(r#"{"src_ip":"1.2.3.4","pool":"p","ttl_sec":5}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "one bad instance must not fail the whole broadcast"
        );
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let results = body["results"].as_array().unwrap();
        let ok = results
            .iter()
            .find(|r| r["instance"] == "reachable")
            .unwrap();
        assert_eq!(ok["status"], 200);
        let bad = results
            .iter()
            .find(|r| r["instance"] == "unreachable")
            .unwrap();
        assert!(bad["status"].is_null());
        assert!(bad["error"].is_string());
    }
}
