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
//!
//! Phase 12 slice 8 (`docs/10` "RBAC and audit (design)"): every verb here
//! logs `(instance(s), actor, verb)` via `tracing` — `actor` is whatever
//! `X-Actor` named (set by `gsp-ui` when it proxies a write; absent for a
//! direct call against this aggregator's own token). Forwarded to each
//! target instance too, for consistency, though no instance reads it today.
//! Deliberately **not** a queryable, durable audit log — this aggregator
//! carries no durable state by design (`crate::ingest`'s doc); a `tracing`
//! line is the proportionate amount of "who did this" for a component whose
//! entire job is relaying, not deciding. The durable half of this release's
//! audit trail is `gsp-controller`'s per-revision `actor` field.

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{patch, post};
use axum::{Json, Router};
use serde::Serialize;

use crate::api::AppState;
use crate::target::Target;

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
        .route(
            "/fleet/instances/{instance}/sniffers",
            axum::routing::get(list_instance_sniffers),
        )
        .route("/fleet/pools/{pool}/backends", post(add_backend))
        .route(
            "/fleet/pools/{pool}/backends/{addr}",
            patch(patch_backend).delete(delete_backend),
        )
        .route("/fleet/route-hint", post(route_hint))
        .route("/fleet/sniffers", post(upload_sniffer))
        .route(
            "/fleet/sniffers/{name}",
            axum::routing::delete(delete_sniffer),
        )
}

fn bad_request(error: String) -> Response {
    (StatusCode::BAD_REQUEST, Json(ErrorResponse { error })).into_response()
}

/// `X-Actor`, if present — see the module doc.
fn actor_header(headers: &HeaderMap) -> Option<String> {
    headers
        .get("X-Actor")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

async fn drain_instance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(instance): Path<String>,
) -> Response {
    let actor = actor_header(&headers);
    tracing::info!(%instance, ?actor, verb = "drain", "fan-out verb");
    proxy_to_instance(
        &state,
        &instance,
        Method::POST,
        Target::new(&["admin", "drain"]),
        None,
        actor.as_deref(),
    )
    .await
}

async fn undrain_instance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(instance): Path<String>,
) -> Response {
    let actor = actor_header(&headers);
    tracing::info!(%instance, ?actor, verb = "undrain", "fan-out verb");
    proxy_to_instance(
        &state,
        &instance,
        Method::POST,
        Target::new(&["admin", "undrain"]),
        None,
        actor.as_deref(),
    )
    .await
}

/// `GET /fleet/instances/{instance}/sniffers` — a targeted read of one named
/// instance's loaded sniffer modules, same targeted shape as drain/undrain
/// (listing has no fleet-wide "merge" meaning across instances with
/// different `settings.sniffers.dir` contents, so a caller picks the
/// instance).
async fn list_instance_sniffers(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(instance): Path<String>,
) -> Response {
    let actor = actor_header(&headers);
    tracing::info!(%instance, ?actor, verb = "list_sniffers", "fan-out verb");
    proxy_to_instance(
        &state,
        &instance,
        Method::GET,
        Target::new(&["admin", "sniffers"]),
        None,
        actor.as_deref(),
    )
    .await
}

async fn add_backend(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(pool): Path<String>,
    body: Bytes,
) -> Response {
    let actor = actor_header(&headers);
    tracing::info!(%pool, ?actor, verb = "add_backend", "fan-out verb");
    broadcast(
        &state,
        Method::POST,
        Target::new(&["pools", &pool, "backends"]),
        Some(body),
        actor.as_deref(),
    )
    .await
}

async fn patch_backend(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((pool, addr)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    let actor = actor_header(&headers);
    tracing::info!(%pool, %addr, ?actor, verb = "patch_backend", "fan-out verb");
    broadcast(
        &state,
        Method::PATCH,
        Target::new(&["pools", &pool, "backends", &addr]),
        Some(body),
        actor.as_deref(),
    )
    .await
}

async fn delete_backend(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((pool, addr)): Path<(String, String)>,
) -> Response {
    let actor = actor_header(&headers);
    tracing::info!(%pool, %addr, ?actor, verb = "delete_backend", "fan-out verb");
    broadcast(
        &state,
        Method::DELETE,
        Target::new(&["pools", &pool, "backends", &addr]),
        None,
        actor.as_deref(),
    )
    .await
}

async fn route_hint(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let actor = actor_header(&headers);
    tracing::info!(?actor, verb = "route_hint", "fan-out verb");
    broadcast(
        &state,
        Method::POST,
        Target::new(&["route-hint"]),
        Some(body),
        actor.as_deref(),
    )
    .await
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

#[derive(serde::Deserialize)]
struct SnifferUploadQuery {
    name: String,
}

/// `POST /fleet/sniffers?name=<module>` — broadcasts a `.wasm` module upload
/// to every known instance's `POST /admin/sniffers`, raw bytes with
/// `content-type: application/octet-stream` (not JSON — this is the one
/// fan-out verb whose body isn't JSON, see `broadcast_with_content_type`).
async fn upload_sniffer(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<SnifferUploadQuery>,
    body: Bytes,
) -> Response {
    let actor = actor_header(&headers);
    tracing::info!(name = %q.name, ?actor, verb = "upload_sniffer", "fan-out verb");
    broadcast_with_content_type(
        &state,
        Method::POST,
        Target::new(&["admin", "sniffers"]).map(|t| t.with_query("name", &q.name)),
        Some(body),
        "application/octet-stream",
        actor.as_deref(),
    )
    .await
}

/// `DELETE /fleet/sniffers/{name}` — broadcasts a module removal to every
/// known instance's `DELETE /admin/sniffers/{name}`.
async fn delete_sniffer(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    let actor = actor_header(&headers);
    tracing::info!(%name, ?actor, verb = "delete_sniffer", "fan-out verb");
    broadcast(
        &state,
        Method::DELETE,
        Target::new(&["admin", "sniffers", &name]),
        None,
        actor.as_deref(),
    )
    .await
}

/// Proxies one call to exactly one named instance, passing its response
/// (status + body) straight through unchanged.
async fn proxy_to_instance(
    state: &AppState,
    instance: &str,
    method: Method,
    target: Result<Target, String>,
    body: Option<Bytes>,
    actor: Option<&str>,
) -> Response {
    let target = match target {
        Ok(t) => t,
        Err(e) => return bad_request(e),
    };
    let Some(inst) = state.store.get(instance) else {
        return (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("unknown instance {instance}"),
            }),
        )
            .into_response();
    };

    let url = match target.url(&inst.payload.admin_url) {
        Ok(url) => url,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse {
                    error: format!("{instance}: {e}"),
                }),
            )
                .into_response()
        }
    };
    let mut req = state.http.request(method, url.clone());
    if let Some(token) = &state.instance_token {
        req = req.bearer_auth(token);
    }
    if let Some(actor) = actor {
        req = req.header("X-Actor", actor);
    }
    if let Some(body) = body {
        req = req.header("content-type", "application/json").body(body);
    }

    match req.send().await {
        Ok(resp) => {
            let status = resp.status();
            let headers = forwardable_headers(resp.headers());
            let body = resp.bytes().await.unwrap_or_default();
            (status, headers, body).into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(ErrorResponse {
                error: format!("{instance} ({url}): {}", gsp_http::error_chain(&e)),
            }),
        )
            .into_response(),
    }
}

/// The upstream response's headers, minus the hop-by-hop ones that don't
/// make sense to forward verbatim (`axum` recomputes `content-length` for
/// the body we actually send; `connection`/`transfer-encoding` describe
/// *this* hop's framing, not the content). Everything else passes through,
/// so a caller reading a response header from this fan-out sees exactly
/// what the target instance sent.
fn forwardable_headers(upstream: &axum::http::HeaderMap) -> axum::http::HeaderMap {
    let mut headers = upstream.clone();
    for h in [
        axum::http::header::CONNECTION,
        axum::http::header::TRANSFER_ENCODING,
        axum::http::header::CONTENT_LENGTH,
    ] {
        headers.remove(h);
    }
    headers
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
    target: Result<Target, String>,
    body: Option<Bytes>,
    actor: Option<&str>,
) -> Response {
    broadcast_with_content_type(state, method, target, body, "application/json", actor).await
}

/// Like [`broadcast`], but lets the caller pick the forwarded body's
/// `content-type` — needed for `/admin/sniffers` uploads, which carry raw
/// `.wasm` bytes rather than JSON.
async fn broadcast_with_content_type(
    state: &AppState,
    method: Method,
    target: Result<Target, String>,
    body: Option<Bytes>,
    content_type: &str,
    actor: Option<&str>,
) -> Response {
    let target = match target {
        Ok(t) => t,
        Err(e) => return bad_request(e),
    };
    let instances = state.store.snapshot();
    let instance_token = state.instance_token.clone();
    let actor = actor.map(str::to_string);
    let content_type = content_type.to_string();
    let mut calls = tokio::task::JoinSet::new();
    for inst in instances {
        let client = state.http.clone();
        let method = method.clone();
        let url = target.url(&inst.payload.admin_url);
        let body = body.clone();
        let instance = inst.payload.instance;
        let instance_token = instance_token.clone();
        let actor = actor.clone();
        let content_type = content_type.clone();
        calls.spawn(async move {
            let url = match url {
                Ok(url) => url,
                Err(error) => {
                    return InstanceResult {
                        instance,
                        status: None,
                        error: Some(error),
                    }
                }
            };
            let mut req = client.request(method, url);
            if let Some(token) = &instance_token {
                req = req.bearer_auth(token);
            }
            if let Some(actor) = &actor {
                req = req.header("X-Actor", actor.as_str());
            }
            if let Some(body) = body {
                req = req.header("content-type", content_type).body(body);
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
                    error: Some(gsp_http::error_chain(&e)),
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
            group: None,
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
    async fn drain_forwards_the_instances_response_headers_too() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mock = Router::new().route(
            "/admin/drain",
            post(|| async { ([("x-drain-note", "graceful")], "draining\n") }),
        );
        tokio::spawn(async move {
            axum::serve(listener, mock).await.unwrap();
        });

        let state = test_state();
        state
            .store
            .ingest(ingest_payload("proxy-1", &format!("http://{addr}")));
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
        assert_eq!(resp.headers().get("x-drain-note").unwrap(), "graceful");
    }

    #[tokio::test]
    async fn drain_forwards_the_x_actor_header_to_the_instance() {
        let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let captured_for_handler = captured.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mock = Router::new().route(
            "/admin/drain",
            post(move |headers: axum::http::HeaderMap| {
                let captured = captured_for_handler.clone();
                async move {
                    *captured.lock().unwrap() = headers
                        .get("X-Actor")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string);
                    "draining\n"
                }
            }),
        );
        tokio::spawn(async move {
            axum::serve(listener, mock).await.unwrap();
        });

        let state = test_state();
        state
            .store
            .ingest(ingest_payload("proxy-1", &format!("http://{addr}")));
        let app = crate::api::router(state);

        app.oneshot(
            Request::post("/fleet/instances/proxy-1/drain")
                .header("X-Actor", "alice")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

        assert_eq!(captured.lock().unwrap().as_deref(), Some("alice"));
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

    type CapturedUpload = Arc<Mutex<Option<(String, Vec<u8>)>>>;

    #[tokio::test]
    async fn upload_sniffer_broadcasts_raw_bytes_with_octet_stream_content_type() {
        let captured: CapturedUpload = Arc::new(Mutex::new(None));
        let captured_for_handler = captured.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mock = Router::new().route(
            "/admin/sniffers",
            post(move |headers: axum::http::HeaderMap, body: Bytes| {
                let captured = captured_for_handler.clone();
                async move {
                    let ct = headers
                        .get("content-type")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_string();
                    *captured.lock().unwrap() = Some((ct, body.to_vec()));
                    "uploaded\n"
                }
            }),
        );
        tokio::spawn(async move {
            axum::serve(listener, mock).await.unwrap();
        });

        let state = test_state();
        state
            .store
            .ingest(ingest_payload("a", &format!("http://{addr}")));
        let app = crate::api::router(state);

        let resp = app
            .oneshot(
                Request::post("/fleet/sniffers?name=demo")
                    .body(Body::from(vec![1u8, 2, 3]))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let (ct, body) = captured.lock().unwrap().clone().unwrap();
        assert_eq!(ct, "application/octet-stream");
        assert_eq!(body, vec![1u8, 2, 3]);
    }

    #[tokio::test]
    async fn delete_sniffer_broadcasts_to_every_instance() {
        let (url, _captured) = spawn_mock_instance(
            "/admin/sniffers/demo",
            Method::DELETE,
            StatusCode::OK,
            "removed\n",
        )
        .await;
        let state = test_state();
        state.store.ingest(ingest_payload("a", &url));
        let app = crate::api::router(state);

        let resp = app
            .oneshot(
                Request::delete("/fleet/sniffers/demo")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["results"][0]["status"], 200);
    }

    /// #111: a decoded `/` or `..` in a path parameter must not steer the
    /// forwarded request to another admin path (carrying the instance token).
    #[tokio::test]
    async fn encoded_traversal_in_a_path_segment_is_refused_before_any_fan_out() {
        let hit: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let hit2 = hit.clone();
        let mock = Router::new().fallback(move |uri: axum::http::Uri| {
            let hit = hit2.clone();
            async move {
                hit.lock().unwrap().push(uri.to_string());
                "ok"
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });

        let state = test_state();
        state
            .store
            .ingest(ingest_payload("a", &format!("http://{addr}")));
        let app = crate::api::router(state);

        for (method, uri) in [
            ("POST", "/fleet/pools/..%2Fadmin%2Fdrain%3F/backends"),
            ("POST", "/fleet/pools/%2e%2e/backends"),
            (
                "PATCH",
                "/fleet/pools/local/backends/..%2F..%2Fadmin%2Fdrain",
            ),
            ("DELETE", "/fleet/pools/local/backends/%2e%2e"),
            ("DELETE", "/fleet/sniffers/..%2Fdrain"),
        ] {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(uri)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{method} {uri}");
        }
        assert!(
            hit.lock().unwrap().is_empty(),
            "nothing reached the instance: {:?}",
            hit.lock().unwrap()
        );
    }

    /// Characters that are legal in a segment but meaningful in a URL are
    /// percent-encoded, so they stay one segment.
    #[tokio::test]
    async fn a_segment_with_a_percent_or_space_stays_one_encoded_segment() {
        let hit: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let hit2 = hit.clone();
        let mock = Router::new().fallback(move |uri: axum::http::Uri| {
            let hit = hit2.clone();
            async move {
                hit.lock().unwrap().push(uri.path().to_string());
                "ok"
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });

        let state = test_state();
        state
            .store
            .ingest(ingest_payload("a", &format!("http://{addr}")));
        let app = crate::api::router(state);
        let resp = app
            .oneshot(
                Request::post("/fleet/pools/a%25b%20c/backends")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            hit.lock().unwrap().as_slice(),
            ["/pools/a%25b%20c/backends"]
        );
    }

    #[tokio::test]
    async fn the_sniffer_name_query_is_encoded() {
        let hit: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let hit2 = hit.clone();
        let mock = Router::new().fallback(move |uri: axum::http::Uri| {
            let hit = hit2.clone();
            async move {
                hit.lock().unwrap().push(uri.to_string());
                "ok"
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });

        let state = test_state();
        state
            .store
            .ingest(ingest_payload("a", &format!("http://{addr}")));
        let app = crate::api::router(state);
        let resp = app
            .oneshot(
                Request::post("/fleet/sniffers?name=a%26x%3D1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            hit.lock().unwrap().as_slice(),
            ["/admin/sniffers?name=a%26x%3D1"]
        );
    }

    /// The fan-out must not follow a redirect: it would carry the instance
    /// token to wherever the instance (or whoever answers for it) points.
    #[tokio::test]
    async fn a_redirect_from_an_instance_is_not_followed() {
        let hit = Arc::new(Mutex::new(0u32));
        let hit2 = hit.clone();
        let target = Router::new().fallback(move || {
            let hit = hit2.clone();
            async move {
                *hit.lock().unwrap() += 1;
                "elsewhere"
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, target).await.unwrap() });

        let redirector = Router::new().fallback(move || async move {
            (
                StatusCode::TEMPORARY_REDIRECT,
                [("location", format!("http://{target_addr}/"))],
            )
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, redirector).await.unwrap() });

        let state = test_state();
        state
            .store
            .ingest(ingest_payload("a", &format!("http://{addr}")));
        let app = crate::api::router(state);
        let resp = app
            .oneshot(
                Request::post("/fleet/instances/a/drain")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(*hit.lock().unwrap(), 0);
    }
}
