//! Browse sniffer registries and install from them (`/api/registries/*`).
//!
//! The registry client lives here in the UI backend, not on the proxies: the
//! backend fetches the index and the artifact, runs every trust check from
//! `wayhouse-registry`, and only then hands the verified bytes to the existing
//! fleet upload, so a proxy never needs internet access and still re-validates
//! what it is given.
//!
//! Reads need `Viewer`; changing the registry list and installing need
//! `Operator`, the same level as uploading a module by hand.

use axum::body::Bytes;
use axum::extract::{Extension, Path, State};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use wayhouse_registry::{
    abi_matches, select, verify_artifact, Environment, Index, SnifferEntry, MAX_MODULE_BYTES,
};

use crate::api::AppState;
use crate::auth::Actor;
use crate::registries::{RegistriesError, RegistryRef};
use crate::registry_client::FetchError;

/// The sniffer ABI the proxies of this release speak, `major.minor`.
fn host_abi() -> String {
    format!(
        "{}.{}",
        wayhouse_sniffer_abi::ABI_MAJOR,
        wayhouse_sniffer_abi::ABI_MINOR
    )
}

pub fn viewer_router() -> Router<AppState> {
    Router::new()
        .route("/api/registries", get(list_registries))
        .route("/api/registries/{id}/sniffers", get(list_sniffers))
}

pub fn operator_router() -> Router<AppState> {
    Router::new()
        .route("/api/registries", post(add_registry))
        .route("/api/registries/{id}", delete(remove_registry))
        .route("/api/registries/{id}/install", post(install))
}

fn error(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({ "error": msg.into() }))).into_response()
}

fn risk(r: &RegistryRef) -> &'static str {
    if r.official {
        "official"
    } else {
        "external"
    }
}

fn registry_json(r: &RegistryRef) -> serde_json::Value {
    json!({ "id": r.id, "name": r.name, "url": r.url, "official": r.official, "risk": risk(r) })
}

async fn list_registries(State(state): State<AppState>) -> Response {
    let list: Vec<_> = state.registries.list().iter().map(registry_json).collect();
    Json(json!({ "registries": list, "persistent": state.registries_persistent })).into_response()
}

#[derive(Deserialize)]
struct AddRegistry {
    url: String,
    #[serde(default)]
    name: Option<String>,
}

async fn add_registry(State(state): State<AppState>, Json(req): Json<AddRegistry>) -> Response {
    match state.registries.add(&req.url, req.name.as_deref()) {
        Ok(r) => Json(registry_json(&r)).into_response(),
        Err(e @ (RegistriesError::BadUrl | RegistriesError::Credentials)) => {
            error(StatusCode::BAD_REQUEST, e.to_string())
        }
        Err(e @ RegistriesError::TooMany) => error(StatusCode::CONFLICT, e.to_string()),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

async fn remove_registry(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state.registries.remove(&id) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => error(StatusCode::NOT_FOUND, "no such registry"),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

fn fetch_failure(e: &FetchError) -> Response {
    error(StatusCode::BAD_GATEWAY, format!("registry: {e}"))
}

/// This release's compatibility environment. The aggregator does not report
/// proxy versions yet, so `min_proxy` cannot be enforced; the listing says so.
fn environment() -> Environment {
    Environment {
        abi: host_abi(),
        proxy_versions: Vec::new(),
    }
}

fn compatible(entry: &SnifferEntry) -> serde_json::Value {
    match select(entry, &environment()) {
        Ok(v) => json!({ "version": v.version.to_string() }),
        Err(why) => json!({ "reason": why.to_string() }),
    }
}

async fn list_sniffers(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let Some(reg) = state.registries.get(&id) else {
        return error(StatusCode::NOT_FOUND, "no such registry");
    };
    let index: std::sync::Arc<Index> = match state.registry_client.fetch_index(&reg.url).await {
        Ok(i) => i,
        Err(e) => return fetch_failure(&e),
    };
    let sniffers: Vec<_> = index
        .sniffers
        .iter()
        .map(|entry| {
            let mut v = serde_json::to_value(entry).expect("index entries serialize");
            v["compatible"] = compatible(entry);
            v
        })
        .collect();
    Json(json!({
        "registry": registry_json(&reg),
        "index_name": index.name,
        "host_abi": host_abi(),
        "min_proxy_checked": false,
        "sniffers": sniffers,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct InstallRequest {
    name: String,
    #[serde(default)]
    version: Option<semver::Version>,
}

#[derive(Deserialize)]
struct FanOut {
    results: Vec<FanOutResult>,
}

#[derive(Deserialize, Serialize)]
struct FanOutResult {
    instance: String,
    status: Option<u16>,
    error: Option<String>,
    /// The start of a refusing proxy's reply; absent from older aggregators.
    #[serde(default)]
    detail: Option<String>,
}

impl FanOutResult {
    /// The proxy pins its sniffers and this module is not on the list. A proxy
    /// answers `409` for that and also when `settings.sniffers` is unset, so the
    /// status alone is not enough: the reply text starts with `pinned:`.
    fn is_pinned(&self) -> bool {
        self.status == Some(409)
            && self
                .detail
                .as_deref()
                .is_some_and(|d| d.starts_with("pinned:"))
    }
}

async fn install(
    State(state): State<AppState>,
    Extension(Actor(actor)): Extension<Actor>,
    Path(id): Path<String>,
    Json(req): Json<InstallRequest>,
) -> Response {
    let Some(reg) = state.registries.get(&id) else {
        return error(StatusCode::NOT_FOUND, "no such registry");
    };
    let index = match state.registry_client.fetch_index(&reg.url).await {
        Ok(i) => i,
        Err(e) => return fetch_failure(&e),
    };
    let Some(entry) = index.sniffers.iter().find(|s| s.name == req.name) else {
        return error(StatusCode::NOT_FOUND, "no such sniffer in this registry");
    };

    let env = environment();
    let version = match &req.version {
        Some(want) => {
            let Some(v) = entry.versions.iter().find(|v| &v.version == want) else {
                return error(StatusCode::NOT_FOUND, "no such version of this sniffer");
            };
            if !abi_matches(&v.abi, &env.abi).unwrap_or(false) {
                return error(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    format!(
                        "version {want} is built for sniffer ABI {}, but this proxy speaks ABI {}",
                        v.abi, env.abi
                    ),
                );
            }
            v
        }
        None => match select(entry, &env) {
            Ok(v) => v,
            Err(why) => return error(StatusCode::UNPROCESSABLE_ENTITY, why.to_string()),
        },
    };

    let cap = version.size.min(MAX_MODULE_BYTES);
    let bytes = match state
        .registry_client
        .fetch_artifact(&version.url, cap)
        .await
    {
        Ok(b) => b,
        Err(e) => return fetch_failure(&e),
    };
    let signature = match &version.signature_url {
        Some(url) => match state.registry_client.fetch_signature(url).await {
            Ok(s) => Some(s),
            Err(e) => return fetch_failure(&e),
        },
        None => None,
    };
    let key = reg
        .official
        .then(crate::registry_keys::official_key)
        .flatten();
    let verified = match verify_artifact(version, &bytes, signature.as_deref(), key.as_ref()) {
        Ok(v) => v,
        Err(e) => return error(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()),
    };

    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    tracing::info!(
        ?actor, registry = %reg.id, sniffer = %entry.name, version = %version.version,
        signed = verified.signed, "installing sniffer from a registry"
    );
    let resp = crate::aggregator_proxy::proxy_raw(
        &state,
        Method::POST,
        &format!("/fleet/sniffers?name={}", entry.name),
        Some(Bytes::from(bytes)),
        "application/octet-stream",
        actor,
    )
    .await;
    let (parts, body) = resp.into_parts();
    let body = axum::body::to_bytes(body, 1024 * 1024)
        .await
        .unwrap_or_default();
    if parts.status != StatusCode::OK {
        // No aggregator configured, or unreachable: nothing was installed.
        return (parts.status, parts.headers, body).into_response();
    }
    let Ok(fan) = serde_json::from_slice::<FanOut>(&body) else {
        return error(
            StatusCode::BAD_GATEWAY,
            "the aggregator's reply was not understood",
        );
    };
    if fan.results.is_empty() {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "no proxies are registered with the aggregator, so nothing was installed",
        );
    }

    let ok = |r: &FanOutResult| r.status.is_some_and(|s| (200..300).contains(&s));
    let pinned: Vec<_> = fan
        .results
        .iter()
        .filter(|r| r.is_pinned())
        .map(|r| json!({ "instance": r.instance, "pin": { "name": entry.name, "sha256": sha256 } }))
        .collect();
    let accepted = fan.results.iter().filter(|r| ok(r)).count();
    // A pinned proxy is not a failed install, but nothing was installed on it
    // either: it needs the operator to add the pin line.
    let status = if accepted == fan.results.len() {
        StatusCode::OK
    } else if accepted + pinned.len() > 0 {
        StatusCode::MULTI_STATUS
    } else {
        StatusCode::BAD_GATEWAY
    };
    let results: Vec<_> = fan
        .results
        .iter()
        .map(|r| {
            json!({
                "instance": r.instance,
                "ok": ok(r),
                "pinned": r.is_pinned(),
                "error": r.error,
                "detail": r.detail,
                "status": r.status,
            })
        })
        .collect();
    (
        status,
        Json(json!({
            "sniffer": entry.name,
            "version": version.version.to_string(),
            "signed": verified.signed,
            "risk": risk(&reg),
            "sha256": sha256,
            "results": results,
            "pinned_instances": pinned,
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registries::Registries;
    use crate::registry_client::RegistryClient;
    use axum::body::Body;
    use axum::http::{header, Request};
    use std::sync::{Arc, Mutex};
    use tower::ServiceExt;

    const MODULE_WAT: &str =
        r#"(module (memory 1) (func (export "f")) (@custom "wayhouse.abi" "\00\00\01\00"))"#;
    const IMPORTING_WAT: &str =
        r#"(module (import "env" "x" (func)) (memory 1) (@custom "wayhouse.abi" "\00\00\01\00"))"#;

    async fn serve(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    /// A registry serving one sniffer `demo` whose artifact is `module`; the index
    /// claims `abi` and `sha256_override` when given.
    async fn fake_registry(module: Vec<u8>, abi: &str, sha256_override: Option<&str>) -> String {
        let sha =
            sha256_override.map_or_else(|| format!("{:x}", Sha256::digest(&module)), str::to_owned);
        let index = json!({
            "schema": 1, "kind": "sniffer", "name": "test registry",
            "sniffers": [{
                "name": "demo", "description": "a demo", "license": "MIT",
                "versions": [{
                    "version": "0.1.0", "abi": abi, "min_proxy": "0.1.0",
                    "url": "https://registry.test/demo.wasm",
                    "sha256": sha, "size": module.len(),
                    "limits": { "max_memory_bytes": 1048576, "call_timeout_ms": 50 }
                }]
            }]
        })
        .to_string();
        serve(
            Router::new()
                .route("/index.json", get(move || async move { index }))
                .route("/demo.wasm", get(move || async move { module })),
        )
        .await
    }

    type Uploads = Arc<Mutex<Vec<Vec<u8>>>>;

    /// A stand-in aggregator answering the fleet upload with `results`.
    async fn fake_aggregator(results: serde_json::Value) -> (String, Uploads) {
        let uploads: Uploads = Arc::default();
        let seen = uploads.clone();
        let app = Router::new().route(
            "/fleet/sniffers",
            post(move |body: Bytes| {
                let seen = seen.clone();
                let results = results.clone();
                async move {
                    seen.lock().unwrap().push(body.to_vec());
                    Json(json!({ "results": results }))
                }
            }),
        );
        (serve(app).await, uploads)
    }

    struct Harness {
        app: Router,
        cookie: String,
        registry_id: String,
    }

    async fn harness(
        registry_base: &str,
        aggregator: &str,
        official: bool,
        role: crate::role::Role,
    ) -> Harness {
        let regs = Arc::new(Registries::load(None, false).unwrap());
        let reg = regs.add_unchecked(&format!("{registry_base}/index.json"), official);
        let state = AppState::new(Some("secret".into()))
            .with_aggregator(aggregator.to_string(), None)
            .with_registries(regs, false)
            .with_registry_client(RegistryClient::for_tests().with_rewrite(registry_base));
        let session = state.sessions.create(crate::session::Session {
            role,
            username: None,
        });
        Harness {
            app: crate::api::router(state),
            cookie: format!("{}={session}", crate::api::SESSION_COOKIE),
            registry_id: reg.id,
        }
    }

    impl Harness {
        async fn call(
            &self,
            method: &str,
            path: &str,
            body: Option<serde_json::Value>,
        ) -> (StatusCode, serde_json::Value) {
            let mut req = Request::builder()
                .method(method)
                .uri(path)
                .header(header::COOKIE, &self.cookie);
            let body = match body {
                Some(b) => {
                    req = req.header(header::CONTENT_TYPE, "application/json");
                    Body::from(b.to_string())
                }
                None => Body::empty(),
            };
            let resp = self
                .app
                .clone()
                .oneshot(req.body(body).unwrap())
                .await
                .unwrap();
            let status = resp.status();
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            (
                status,
                serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
            )
        }

        async fn install(&self) -> (StatusCode, serde_json::Value) {
            self.call(
                "POST",
                &format!("/api/registries/{}/install", self.registry_id),
                Some(json!({ "name": "demo" })),
            )
            .await
        }
    }

    fn ok_results() -> serde_json::Value {
        json!([{ "instance": "a", "status": 200, "error": null }, { "instance": "b", "status": 200, "error": null }])
    }

    #[tokio::test]
    async fn lists_registries_with_the_risk_flag() {
        let h = harness(
            "http://127.0.0.1:1",
            "http://127.0.0.1:1",
            false,
            crate::role::Role::Viewer,
        )
        .await;
        let (status, body) = h.call("GET", "/api/registries", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["registries"][0]["risk"], "external");
        assert_eq!(body["persistent"], false);
    }

    #[tokio::test]
    async fn official_registries_are_not_flagged_external() {
        let h = harness(
            "http://127.0.0.1:1",
            "http://127.0.0.1:1",
            true,
            crate::role::Role::Viewer,
        )
        .await;
        let (_, body) = h.call("GET", "/api/registries", None).await;
        assert_eq!(body["registries"][0]["risk"], "official");
    }

    #[tokio::test]
    async fn add_then_list_then_delete() {
        let h = harness(
            "http://127.0.0.1:1",
            "http://127.0.0.1:1",
            false,
            crate::role::Role::Operator,
        )
        .await;
        let (status, added) = h
            .call(
                "POST",
                "/api/registries",
                Some(json!({ "url": "https://example.com/index.json", "name": "mine" })),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(added["risk"], "external");
        let id = added["id"].as_str().unwrap().to_string();
        let (_, listed) = h.call("GET", "/api/registries", None).await;
        assert_eq!(listed["registries"].as_array().unwrap().len(), 2);
        let (status, _) = h
            .call("DELETE", &format!("/api/registries/{id}"), None)
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _) = h
            .call("DELETE", &format!("/api/registries/{id}"), None)
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn adding_a_plain_http_registry_is_refused() {
        let h = harness(
            "http://127.0.0.1:1",
            "http://127.0.0.1:1",
            false,
            crate::role::Role::Operator,
        )
        .await;
        let (status, body) = h
            .call(
                "POST",
                "/api/registries",
                Some(json!({ "url": "http://example.com/index.json" })),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].as_str().unwrap().contains("https"));
    }

    #[tokio::test]
    async fn mutating_routes_need_the_operator_role() {
        let h = harness(
            "http://127.0.0.1:1",
            "http://127.0.0.1:1",
            false,
            crate::role::Role::Viewer,
        )
        .await;
        let (status, _) = h
            .call(
                "POST",
                "/api/registries",
                Some(json!({ "url": "https://example.com/i.json" })),
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) = h
            .call(
                "DELETE",
                &format!("/api/registries/{}", h.registry_id),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) = h.install().await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn sniffer_listing_says_compatible_and_that_min_proxy_was_not_checked() {
        let module = wat::parse_str(MODULE_WAT).unwrap();
        let base = fake_registry(module, "0.1", None).await;
        let h = harness(
            &base,
            "http://127.0.0.1:1",
            false,
            crate::role::Role::Viewer,
        )
        .await;
        let (status, body) = h
            .call(
                "GET",
                &format!("/api/registries/{}/sniffers", h.registry_id),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["sniffers"][0]["name"], "demo");
        assert_eq!(body["sniffers"][0]["compatible"]["version"], "0.1.0");
        assert_eq!(body["min_proxy_checked"], false);
    }

    #[tokio::test]
    async fn sniffer_listing_marks_an_abi_mismatch_with_its_reason() {
        let module = wat::parse_str(MODULE_WAT).unwrap();
        let base = fake_registry(module, "0.9", None).await;
        let h = harness(
            &base,
            "http://127.0.0.1:1",
            false,
            crate::role::Role::Viewer,
        )
        .await;
        let (_, body) = h
            .call(
                "GET",
                &format!("/api/registries/{}/sniffers", h.registry_id),
                None,
            )
            .await;
        let reason = body["sniffers"][0]["compatible"]["reason"]
            .as_str()
            .unwrap();
        assert!(reason.contains("0.9") && reason.contains("0.1"), "{reason}");
    }

    #[tokio::test]
    async fn unknown_registry_is_404_everywhere() {
        let h = harness(
            "http://127.0.0.1:1",
            "http://127.0.0.1:1",
            false,
            crate::role::Role::Operator,
        )
        .await;
        let (status, _) = h.call("GET", "/api/registries/nope/sniffers", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = h
            .call(
                "POST",
                "/api/registries/nope/install",
                Some(json!({ "name": "demo" })),
            )
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn install_happy_path_uploads_the_verified_bytes() {
        let module = wat::parse_str(MODULE_WAT).unwrap();
        let base = fake_registry(module.clone(), "0.1", None).await;
        let (agg, uploads) = fake_aggregator(ok_results()).await;
        let h = harness(&base, &agg, false, crate::role::Role::Operator).await;
        let (status, body) = h.install().await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["version"], "0.1.0");
        assert_eq!(body["signed"], false);
        assert_eq!(body["risk"], "external");
        assert_eq!(body["sha256"], format!("{:x}", Sha256::digest(&module)));
        assert_eq!(body["results"].as_array().unwrap().len(), 2);
        assert_eq!(uploads.lock().unwrap().as_slice(), [module]);
    }

    #[tokio::test]
    async fn install_refuses_a_sha_mismatch_and_uploads_nothing() {
        let module = wat::parse_str(MODULE_WAT).unwrap();
        let wrong = "0".repeat(64);
        let base = fake_registry(module, "0.1", Some(&wrong)).await;
        let (agg, uploads) = fake_aggregator(ok_results()).await;
        let h = harness(&base, &agg, false, crate::role::Role::Operator).await;
        let (status, body) = h.install().await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert!(uploads.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn install_refuses_an_abi_mismatch_and_uploads_nothing() {
        let module = wat::parse_str(MODULE_WAT).unwrap();
        let base = fake_registry(module, "0.9", None).await;
        let (agg, uploads) = fake_aggregator(ok_results()).await;
        let h = harness(&base, &agg, false, crate::role::Role::Operator).await;
        let (status, body) = h.install().await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert!(body["error"].as_str().unwrap().contains("ABI"));
        assert!(uploads.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn install_refuses_a_module_with_an_import_and_uploads_nothing() {
        let module = wat::parse_str(IMPORTING_WAT).unwrap();
        let base = fake_registry(module, "0.1", None).await;
        let (agg, uploads) = fake_aggregator(ok_results()).await;
        let h = harness(&base, &agg, false, crate::role::Role::Operator).await;
        let (status, _) = h.install().await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(uploads.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn install_reports_a_partial_fanout_as_207_with_the_failed_instance() {
        let module = wat::parse_str(MODULE_WAT).unwrap();
        let base = fake_registry(module, "0.1", None).await;
        let (agg, _) = fake_aggregator(json!([
            { "instance": "a", "status": 200, "error": null },
            { "instance": "b", "status": null, "error": "connection refused" },
        ]))
        .await;
        let h = harness(&base, &agg, false, crate::role::Role::Operator).await;
        let (status, body) = h.install().await;
        assert_eq!(status, StatusCode::MULTI_STATUS);
        assert_eq!(body["results"][1]["ok"], false);
        assert_eq!(body["results"][1]["error"], "connection refused");
    }

    #[tokio::test]
    async fn a_pinned_instance_is_reported_with_its_pin_line() {
        let module = wat::parse_str(MODULE_WAT).unwrap();
        let base = fake_registry(module.clone(), "0.1", None).await;
        let (agg, _) = fake_aggregator(json!([
            { "instance": "a", "status": 200, "error": null },
            { "instance": "b", "status": 409, "error": null, "detail": "pinned: demo is not listed" },
        ]))
        .await;
        let h = harness(&base, &agg, false, crate::role::Role::Operator).await;
        let (status, body) = h.install().await;
        assert_eq!(status, StatusCode::MULTI_STATUS);
        assert_eq!(body["results"][1]["pinned"], true);
        assert_eq!(body["pinned_instances"][0]["instance"], "b");
        assert_eq!(body["pinned_instances"][0]["pin"]["name"], "demo");
        assert_eq!(
            body["pinned_instances"][0]["pin"]["sha256"],
            format!("{:x}", Sha256::digest(&module))
        );
    }

    #[tokio::test]
    async fn a_409_that_is_not_a_pin_is_not_reported_as_pinned() {
        let module = wat::parse_str(MODULE_WAT).unwrap();
        let base = fake_registry(module, "0.1", None).await;
        let (agg, _) = fake_aggregator(json!([
            { "instance": "a", "status": 200, "error": null },
            { "instance": "b", "status": 409, "error": null,
              "detail": "settings.sniffers is not configured on this instance; turning it on needs a restart" },
        ]))
        .await;
        let h = harness(&base, &agg, false, crate::role::Role::Operator).await;
        let (status, body) = h.install().await;
        assert_eq!(status, StatusCode::MULTI_STATUS);
        assert_eq!(body["results"][1]["pinned"], false);
        assert_eq!(body["results"][1]["ok"], false);
        assert!(body["results"][1]["detail"]
            .as_str()
            .unwrap()
            .contains("not configured"));
        assert!(body["pinned_instances"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_fleet_that_is_all_pinned_is_a_207_not_a_502() {
        let module = wat::parse_str(MODULE_WAT).unwrap();
        let base = fake_registry(module, "0.1", None).await;
        let (agg, _) = fake_aggregator(json!([
            { "instance": "a", "status": 409, "error": null, "detail": "pinned: demo is not listed" },
            { "instance": "b", "status": 409, "error": null, "detail": "pinned: demo is not listed" },
        ]))
        .await;
        let h = harness(&base, &agg, false, crate::role::Role::Operator).await;
        let (status, body) = h.install().await;
        assert_eq!(status, StatusCode::MULTI_STATUS);
        assert_eq!(body["pinned_instances"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn install_with_nothing_registered_says_so_instead_of_succeeding() {
        let module = wat::parse_str(MODULE_WAT).unwrap();
        let base = fake_registry(module, "0.1", None).await;
        let (agg, _) = fake_aggregator(json!([])).await;
        let h = harness(&base, &agg, false, crate::role::Role::Operator).await;
        let (status, _) = h.install().await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn an_unreachable_registry_is_a_502() {
        let h = harness(
            "http://127.0.0.1:1",
            "http://127.0.0.1:1",
            false,
            crate::role::Role::Operator,
        )
        .await;
        let (status, _) = h.install().await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
    }
}
