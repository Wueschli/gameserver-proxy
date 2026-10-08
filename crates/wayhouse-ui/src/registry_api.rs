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
    abi_matches, check_min_proxy, select, verify_artifact, Environment, Index, SnifferEntry,
    MAX_MODULE_BYTES,
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
        .route("/api/registries/updates/check", post(check_updates))
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

/// What a sniffer will be installed into: this release's ABI and the version of
/// every proxy the aggregator knows.
struct FleetEnv {
    env: Environment,
    /// Every registered proxy reported a version, so `min_proxy` is enforced
    /// for all of them. False when the aggregator is unreachable, knows no
    /// proxy, or one runs a build that reports no (or an unreadable) version;
    /// the versions that are known are still checked.
    min_proxy_checked: bool,
}

/// The ABI-only environment, for when the fleet's versions cannot be learned.
fn unchecked() -> FleetEnv {
    FleetEnv {
        env: Environment {
            abi: host_abi(),
            proxy_versions: Vec::new(),
        },
        min_proxy_checked: false,
    }
}

/// Read the proxy versions from the aggregator's `/fleet/healthz`. Stale
/// instances count: the install still fans out to them.
async fn fleet_env(state: &AppState, actor: Option<String>) -> FleetEnv {
    let resp = crate::aggregator_proxy::proxy_raw(
        state,
        Method::GET,
        "/fleet/healthz",
        None,
        "application/json",
        actor,
    )
    .await;
    let (parts, body) = resp.into_parts();
    if parts.status != StatusCode::OK {
        return unchecked();
    }
    let body = axum::body::to_bytes(body, 1024 * 1024)
        .await
        .unwrap_or_default();
    match serde_json::from_slice::<Vec<HealthEntry>>(&body) {
        Ok(instances) => fleet_env_from(&instances),
        Err(_) => unchecked(),
    }
}

fn fleet_env_from(instances: &[HealthEntry]) -> FleetEnv {
    let versions: Vec<_> = instances
        .iter()
        .map(|i| semver::Version::parse(&i.version).ok())
        .collect();
    FleetEnv {
        min_proxy_checked: !versions.is_empty() && versions.iter().all(Option::is_some),
        env: Environment {
            abi: host_abi(),
            proxy_versions: versions.into_iter().flatten().collect(),
        },
    }
}

fn compatible(entry: &SnifferEntry, env: &Environment) -> serde_json::Value {
    match select(entry, env) {
        Ok(v) => json!({ "version": v.version.to_string() }),
        Err(why) => json!({ "reason": why.to_string() }),
    }
}

async fn list_sniffers(
    State(state): State<AppState>,
    Extension(Actor(actor)): Extension<Actor>,
    Path(id): Path<String>,
) -> Response {
    let Some(reg) = state.registries.get(&id) else {
        return error(StatusCode::NOT_FOUND, "no such registry");
    };
    let index: std::sync::Arc<Index> = match state.registry_client.fetch_index(&reg.url).await {
        Ok(i) => i,
        Err(e) => return fetch_failure(&e),
    };
    let fleet = fleet_env(&state, actor).await;
    let sniffers: Vec<_> = index
        .sniffers
        .iter()
        .map(|entry| {
            let mut v = serde_json::to_value(entry).expect("index entries serialize");
            v["compatible"] = compatible(entry, &fleet.env);
            v
        })
        .collect();
    Json(json!({
        "registry": registry_json(&reg),
        "index_name": index.name,
        "host_abi": host_abi(),
        "min_proxy_checked": fleet.min_proxy_checked,
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

    let env = fleet_env(&state, actor.clone()).await.env;
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
            if let Err(why) = check_min_proxy(v, &env) {
                return error(StatusCode::UNPROCESSABLE_ENTITY, why.to_string());
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

/// `name` as one URL path segment: everything but unreserved characters is
/// percent-encoded, so a `/`, `?`, `#` or `%` in an instance name cannot change
/// the route.
fn path_segment(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// One instance's `GET /admin/sniffers` row, as far as the check needs it.
#[derive(Deserialize)]
struct InstalledSniffer {
    name: String,
    sha256: String,
    #[serde(default)]
    has_previous: bool,
    #[serde(default)]
    fallback: bool,
}

#[derive(Deserialize)]
struct HealthEntry {
    instance: String,
    /// Empty from an aggregator or proxy build that predates the field.
    #[serde(default)]
    version: String,
}

/// Instances that run one build of one sniffer.
#[derive(Default)]
struct InstalledGroup {
    instances: Vec<String>,
    has_previous: bool,
    fallback: bool,
}

/// A newer version of an installed sniffer, from one registry.
struct Update {
    registry_id: String,
    version: semver::Version,
    compatible: bool,
    reason: Option<String>,
}

/// Why the newest version cannot be installed here, or `None` if it can.
fn update_from(
    reg: &RegistryRef,
    entry: &SnifferEntry,
    installed: &semver::Version,
    env: &Environment,
) -> Option<Update> {
    let newest = entry
        .versions
        .iter()
        .max_by(|a, b| a.version.cmp(&b.version))?;
    if newest.version <= *installed {
        return None;
    }
    let pick = |compatible, version: &semver::Version, reason| Update {
        registry_id: reg.id.clone(),
        version: version.clone(),
        compatible,
        reason,
    };
    match select(entry, env) {
        Ok(v) if v.version > *installed => Some(pick(true, &v.version, None)),
        other => {
            let reason = if abi_matches(&newest.abi, &env.abi).unwrap_or(false) {
                match other {
                    Err(why) => why.to_string(),
                    Ok(_) => "no newer version is compatible with this release".to_string(),
                }
            } else {
                format!(
                    "version {} is built for sniffer ABI {}, but this proxy speaks ABI {}",
                    newest.version, newest.abi, env.abi
                )
            };
            Some(pick(false, &newest.version, Some(reason)))
        }
    }
}

/// `POST /api/registries/updates/check` — on demand only, never on a timer.
/// Refetches every registry's index (a cached one would hide a release), asks
/// each proxy what it has installed, and maps each installed build's sha256 to
/// a version in the indexes. A build in no index is "unknown" and gets no
/// update: guessing from a name would offer to replace something hand-built.
/// An unreachable registry or proxy is reported and the rest still answers.
async fn check_updates(
    State(state): State<AppState>,
    Extension(Actor(actor)): Extension<Actor>,
) -> Response {
    let mut set = tokio::task::JoinSet::new();
    for (n, reg) in state.registries.list().into_iter().enumerate() {
        let client = state.registry_client.clone();
        set.spawn(async move {
            client.invalidate(&reg.url);
            let index = client.fetch_index(&reg.url).await;
            (n, reg, index)
        });
    }
    let mut fetched = Vec::new();
    while let Some(done) = set.join_next().await {
        if let Ok(r) = done {
            fetched.push(r);
        }
    }
    fetched.sort_by_key(|(n, ..)| *n);
    let fetched: Vec<_> = fetched.into_iter().map(|(_, r, i)| (r, i)).collect();
    let mut registries = Vec::new();
    let mut indexes: Vec<(RegistryRef, std::sync::Arc<Index>)> = Vec::new();
    for (reg, index) in fetched {
        match index {
            Ok(i) => {
                registries.push(json!({ "id": reg.id, "name": reg.name, "ok": true }));
                indexes.push((reg, i));
            }
            Err(e) => registries.push(
                json!({ "id": reg.id, "name": reg.name, "ok": false, "error": e.to_string() }),
            ),
        }
    }

    let health = crate::aggregator_proxy::proxy_raw(
        &state,
        Method::GET,
        "/fleet/healthz",
        None,
        "application/json",
        actor.clone(),
    )
    .await;
    let (parts, body) = health.into_parts();
    let body = axum::body::to_bytes(body, 1024 * 1024)
        .await
        .unwrap_or_default();
    if parts.status != StatusCode::OK {
        return (parts.status, parts.headers, body).into_response();
    }
    let Ok(mut instances) = serde_json::from_slice::<Vec<HealthEntry>>(&body) else {
        return error(
            StatusCode::BAD_GATEWAY,
            "the aggregator's reply was not understood",
        );
    };
    instances.sort_by(|a, b| a.instance.cmp(&b.instance));
    let fleet = fleet_env_from(&instances);

    // One request per instance at once, so a slow proxy delays the check by its
    // own timeout, not the sum of them. Results are merged in `instances` order.
    let mut set = tokio::task::JoinSet::new();
    for (n, inst) in instances.iter().enumerate() {
        let state = state.clone();
        let actor = actor.clone();
        let name = inst.instance.clone();
        set.spawn(async move {
            let resp = crate::aggregator_proxy::proxy_raw(
                &state,
                Method::GET,
                &format!("/fleet/instances/{}/sniffers", path_segment(&name)),
                None,
                "application/json",
                actor,
            )
            .await;
            let (parts, body) = resp.into_parts();
            let body = axum::body::to_bytes(body, 8 * 1024 * 1024)
                .await
                .unwrap_or_default();
            (n, parts.status, body)
        });
    }
    let mut answers = Vec::new();
    while let Some(done) = set.join_next().await {
        if let Ok(r) = done {
            answers.push(r);
        }
    }
    answers.sort_by_key(|(n, ..)| *n);

    let mut groups: std::collections::BTreeMap<(String, String), InstalledGroup> =
        std::collections::BTreeMap::new();
    let mut instance_errors = Vec::new();
    for (n, status, body) in answers {
        let inst = &instances[n];
        let listed = (status == StatusCode::OK)
            .then(|| serde_json::from_slice::<Vec<InstalledSniffer>>(&body).ok())
            .flatten();
        let Some(listed) = listed else {
            instance_errors.push(json!({
                "instance": inst.instance,
                "status": status.as_u16(),
                "detail": String::from_utf8_lossy(&body).chars().take(200).collect::<String>(),
            }));
            continue;
        };
        for s in listed {
            let g = groups.entry((s.name, s.sha256)).or_default();
            g.instances.push(inst.instance.clone());
            g.has_previous |= s.has_previous;
            g.fallback |= s.fallback;
        }
    }

    let rows: Vec<_> = groups
        .into_iter()
        .map(|((name, sha256), g)| {
            let known = indexes.iter().find_map(|(_, index)| {
                index
                    .sniffers
                    .iter()
                    .filter(|e| e.name == name)
                    .flat_map(|e| &e.versions)
                    .find(|v| v.sha256.eq_ignore_ascii_case(&sha256))
                    .map(|v| v.version.clone())
            });
            let update = known.as_ref().and_then(|installed| {
                indexes
                    .iter()
                    .filter_map(|(reg, index)| {
                        let entry = index.sniffers.iter().find(|e| e.name == name)?;
                        update_from(reg, entry, installed, &fleet.env)
                    })
                    .max_by(|a, b| (a.compatible, &a.version).cmp(&(b.compatible, &b.version)))
            });
            json!({
                "sniffer": name,
                "installed_sha256": sha256,
                "installed_version": known.as_ref().map(ToString::to_string),
                "known": known.is_some(),
                "has_previous": g.has_previous,
                "fallback": g.fallback,
                "update": update.map(|u| json!({
                    "registry_id": u.registry_id,
                    "version": u.version.to_string(),
                    "compatible": u.compatible,
                    "reason": u.reason,
                })),
                "instances": g.instances,
            })
        })
        .collect();
    Json(json!({
        "host_abi": host_abi(),
        "min_proxy_checked": fleet.min_proxy_checked,
        "registries": registries,
        "sniffers": rows,
        "instance_errors": instance_errors,
    }))
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
        fake_aggregator_with_health(results, None).await
    }

    /// As [`fake_aggregator`], and serving `/fleet/healthz` with `health` when given.
    async fn fake_aggregator_with_health(
        results: serde_json::Value,
        health: Option<serde_json::Value>,
    ) -> (String, Uploads) {
        let uploads: Uploads = Arc::default();
        let seen = uploads.clone();
        let mut app = Router::new().route(
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
        if let Some(health) = health {
            app = app.route(
                "/fleet/healthz",
                get(move || {
                    let health = health.clone();
                    async move { Json(health) }
                }),
            );
        }
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

    fn health(versions: &[&str]) -> serde_json::Value {
        let rows: Vec<_> = versions
            .iter()
            .enumerate()
            .map(|(i, v)| {
                json!({ "instance": format!("p{i}"), "last_seen_ms_ago": 1, "stale": false, "version": v })
            })
            .collect();
        json!(rows)
    }

    #[tokio::test]
    async fn sniffer_listing_enforces_min_proxy_against_the_fleet_versions() {
        let module = wat::parse_str(MODULE_WAT).unwrap();
        let base = fake_registry(module, "0.1", None).await;
        // The demo sniffer needs proxy 0.1.0; one proxy still runs 0.0.9.
        let (agg, _) =
            fake_aggregator_with_health(json!([]), Some(health(&["0.1.0", "0.0.9"]))).await;
        let h = harness(&base, &agg, false, crate::role::Role::Viewer).await;
        let (_, body) = h
            .call(
                "GET",
                &format!("/api/registries/{}/sniffers", h.registry_id),
                None,
            )
            .await;
        assert_eq!(body["min_proxy_checked"], true);
        let reason = body["sniffers"][0]["compatible"]["reason"]
            .as_str()
            .unwrap();
        assert!(
            reason.contains("0.1.0") && reason.contains("0.0.9"),
            "{reason}"
        );
    }

    #[tokio::test]
    async fn sniffer_listing_with_a_proxy_that_reports_no_version_is_not_fully_checked() {
        let module = wat::parse_str(MODULE_WAT).unwrap();
        let base = fake_registry(module, "0.1", None).await;
        let (agg, _) = fake_aggregator_with_health(json!([]), Some(health(&["0.1.0", ""]))).await;
        let h = harness(&base, &agg, false, crate::role::Role::Viewer).await;
        let (_, body) = h
            .call(
                "GET",
                &format!("/api/registries/{}/sniffers", h.registry_id),
                None,
            )
            .await;
        assert_eq!(body["min_proxy_checked"], false);
        assert_eq!(body["sniffers"][0]["compatible"]["version"], "0.1.0");
    }

    #[tokio::test]
    async fn install_refuses_a_version_a_proxy_is_too_old_for_and_uploads_nothing() {
        let module = wat::parse_str(MODULE_WAT).unwrap();
        let base = fake_registry(module, "0.1", None).await;
        let (agg, uploads) = fake_aggregator_with_health(json!([]), Some(health(&["0.0.9"]))).await;
        let h = harness(&base, &agg, false, crate::role::Role::Operator).await;
        let (status, body) = h.install().await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(body["error"].as_str().unwrap().contains("0.0.9"), "{body}");
        // Naming the version does not get around the check.
        let (status, _) = install_version(&h, "0.1.0").await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(uploads.lock().unwrap().is_empty());
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

    /// A registry serving `demo` in several versions `(version, abi, module)`;
    /// each artifact lives at `/demo-<version>.wasm`.
    async fn fake_registry_versions(versions: &[(&str, &str, Vec<u8>)]) -> String {
        let list: Vec<_> = versions
            .iter()
            .map(|(v, abi, module)| {
                json!({
                    "version": v, "abi": abi, "min_proxy": "0.1.0",
                    "url": format!("https://registry.test/demo-{v}.wasm"),
                    "sha256": format!("{:x}", Sha256::digest(module)), "size": module.len(),
                    "limits": { "max_memory_bytes": 1048576, "call_timeout_ms": 50 }
                })
            })
            .collect();
        let index = json!({
            "schema": 1, "kind": "sniffer", "name": "test registry",
            "sniffers": [{ "name": "demo", "description": "d", "license": "MIT", "versions": list }]
        })
        .to_string();
        let mut app = Router::new().route("/index.json", get(move || async move { index }));
        for (v, _, module) in versions {
            let module = module.clone();
            app = app.route(
                &format!("/demo-{v}.wasm"),
                get(move || async move { module }),
            );
        }
        serve(app).await
    }

    /// A module distinguishable from the others by a custom section named `tag`.
    fn tagged_module(tag: &str) -> Vec<u8> {
        wat::parse_str(format!(
            r#"(module (memory 1) (func (export "f")) (@custom "wayhouse.abi" "\00\00\01\00") (@custom "{tag}" ""))"#
        ))
        .unwrap()
    }

    async fn install_version(h: &Harness, version: &str) -> (StatusCode, serde_json::Value) {
        h.call(
            "POST",
            &format!("/api/registries/{}/install", h.registry_id),
            Some(json!({ "name": "demo", "version": version })),
        )
        .await
    }

    #[tokio::test]
    async fn install_with_a_version_installs_that_one_even_when_a_newer_exists() {
        let (old, new) = (tagged_module("old"), tagged_module("new"));
        let base =
            fake_registry_versions(&[("0.2.0", "0.1", new.clone()), ("0.1.0", "0.1", old.clone())])
                .await;
        let (agg, uploads) = fake_aggregator(ok_results()).await;
        let h = harness(&base, &agg, false, crate::role::Role::Operator).await;
        let (status, body) = install_version(&h, "0.1.0").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["version"], "0.1.0");
        assert_eq!(uploads.lock().unwrap().as_slice(), [old]);
        // Without a version the newest compatible one is installed.
        let (status, body) = h.install().await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["version"], "0.2.0");
    }

    #[tokio::test]
    async fn install_with_an_unknown_version_is_404_and_uploads_nothing() {
        let base = fake_registry_versions(&[("0.1.0", "0.1", tagged_module("old"))]).await;
        let (agg, uploads) = fake_aggregator(ok_results()).await;
        let h = harness(&base, &agg, false, crate::role::Role::Operator).await;
        let (status, body) = install_version(&h, "9.9.9").await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert!(uploads.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn install_with_a_version_of_another_abi_is_422_and_uploads_nothing() {
        let base = fake_registry_versions(&[
            ("0.2.0", "0.9", tagged_module("new")),
            ("0.1.0", "0.1", tagged_module("old")),
        ])
        .await;
        let (agg, uploads) = fake_aggregator(ok_results()).await;
        let h = harness(&base, &agg, false, crate::role::Role::Operator).await;
        let (status, body) = install_version(&h, "0.2.0").await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert!(body["error"].as_str().unwrap().contains("ABI"));
        assert!(uploads.lock().unwrap().is_empty());
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

    // ---- on-demand update check (#184) ----

    fn sha_of(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    /// Hits on `/index.json` of a registry serving `demo` with `versions`
    /// `(version, abi, sha256)`, newest first.
    async fn registry_with_versions(
        versions: &[(&str, &str, String)],
    ) -> (String, Arc<Mutex<u32>>) {
        let hits = Arc::new(Mutex::new(0u32));
        let counted = hits.clone();
        let list: Vec<_> = versions
            .iter()
            .map(|(v, abi, sha)| {
                json!({
                    "version": v, "abi": abi, "min_proxy": "0.1.0",
                    "url": "https://registry.test/demo.wasm",
                    "sha256": sha, "size": 10,
                    "limits": { "max_memory_bytes": 1048576, "call_timeout_ms": 50 }
                })
            })
            .collect();
        let index = json!({
            "schema": 1, "kind": "sniffer", "name": "test registry",
            "sniffers": [{ "name": "demo", "description": "d", "license": "MIT", "versions": list }]
        })
        .to_string();
        let app = Router::new().route(
            "/index.json",
            get(move || {
                let index = index.clone();
                let counted = counted.clone();
                async move {
                    *counted.lock().unwrap() += 1;
                    index
                }
            }),
        );
        (serve(app).await, hits)
    }

    /// An aggregator that knows `instances` `(name, sniffer listing)`.
    async fn fake_fleet(instances: serde_json::Value) -> String {
        fake_fleet_at(instances, None).await
    }

    /// As [`fake_fleet`], every instance reporting product `version`.
    async fn fake_fleet_at(instances: serde_json::Value, version: Option<&str>) -> String {
        let health: Vec<_> = instances
            .as_object()
            .unwrap()
            .keys()
            .map(|k| {
                json!({ "instance": k, "last_seen_ms_ago": 1, "stale": false,
                        "version": version.unwrap_or("") })
            })
            .collect();
        let app = Router::new()
            .route(
                "/fleet/healthz",
                get(move || {
                    let health = health.clone();
                    async move { Json(health) }
                }),
            )
            .route(
                "/fleet/instances/{instance}/sniffers",
                get(move |Path(i): Path<String>| {
                    let instances = instances.clone();
                    async move {
                        match instances.get(&i) {
                            Some(v) => (StatusCode::OK, Json(v.clone())).into_response(),
                            None => StatusCode::NOT_FOUND.into_response(),
                        }
                    }
                }),
            );
        serve(app).await
    }

    fn listing(sha: &str) -> serde_json::Value {
        json!([{ "name": "demo", "sha256": sha, "size_bytes": 10, "loaded": true,
                 "has_previous": false, "fallback": false }])
    }

    /// A UI whose registry list is `bases` (each an `/index.json` server).
    fn checker(bases: &[&str], aggregator: &str) -> Harness {
        let regs = Arc::new(Registries::load(None, false).unwrap());
        let mut first = String::new();
        for b in bases {
            let r = regs.add_unchecked(&format!("{b}/index.json"), false);
            if first.is_empty() {
                first = r.id;
            }
        }
        let state = AppState::new(Some("secret".into()))
            .with_aggregator(aggregator.to_string(), None)
            .with_registries(regs, false)
            .with_registry_client(RegistryClient::for_tests());
        let session = state.sessions.create(crate::session::Session {
            role: crate::role::Role::Operator,
            username: None,
        });
        Harness {
            app: crate::api::router(state),
            cookie: format!("{}={session}", crate::api::SESSION_COOKIE),
            registry_id: first,
        }
    }

    async fn check(h: &Harness) -> (StatusCode, serde_json::Value) {
        h.call("POST", "/api/registries/updates/check", None).await
    }

    #[tokio::test]
    async fn check_finds_newer_compatible_version() {
        let old = sha_of(b"v1");
        let (reg, _) = registry_with_versions(&[
            ("0.2.0", "0.1", sha_of(b"v2")),
            ("0.1.0", "0.1", old.clone()),
        ])
        .await;
        let fleet = fake_fleet(json!({ "a": listing(&old) })).await;
        let (status, body) = check(&checker(&[&reg], &fleet)).await;
        assert_eq!(status, StatusCode::OK);
        let row = &body["sniffers"][0];
        assert_eq!(row["sniffer"], "demo");
        assert_eq!(row["known"], true);
        assert_eq!(row["installed_version"], "0.1.0");
        assert_eq!(row["update"]["version"], "0.2.0");
        assert_eq!(row["update"]["compatible"], true);
        assert!(row["update"]["registry_id"].is_string());
        assert_eq!(row["instances"], json!(["a"]));
    }

    #[tokio::test]
    async fn check_marks_unknown_build_without_update() {
        let (reg, _) = registry_with_versions(&[("0.2.0", "0.1", sha_of(b"v2"))]).await;
        let fleet = fake_fleet(json!({ "a": listing(&sha_of(b"hand built")) })).await;
        let (_, body) = check(&checker(&[&reg], &fleet)).await;
        let row = &body["sniffers"][0];
        assert_eq!(row["known"], false);
        assert!(row["update"].is_null(), "{row}");
        assert!(row["installed_version"].is_null());
    }

    #[tokio::test]
    async fn check_ignores_incompatible_newer_version_but_reports_reason() {
        let old = sha_of(b"v1");
        let (reg, _) = registry_with_versions(&[
            ("0.3.0", "9.0", sha_of(b"v3")),
            ("0.1.0", "0.1", old.clone()),
        ])
        .await;
        let fleet = fake_fleet(json!({ "a": listing(&old) })).await;
        let (_, body) = check(&checker(&[&reg], &fleet)).await;
        let update = &body["sniffers"][0]["update"];
        assert_eq!(update["version"], "0.3.0");
        assert_eq!(update["compatible"], false);
        assert!(
            update["reason"].as_str().unwrap().contains("ABI"),
            "{update}"
        );
    }

    #[tokio::test]
    async fn check_marks_an_update_a_proxy_is_too_old_for_as_incompatible() {
        let old = sha_of(b"v1");
        // Both versions need proxy 0.1.0 (see `registry_with_versions`).
        let (reg, _) = registry_with_versions(&[
            ("0.2.0", "0.1", sha_of(b"v2")),
            ("0.1.0", "0.1", old.clone()),
        ])
        .await;
        let fleet = fake_fleet_at(json!({ "a": listing(&old) }), Some("0.0.9")).await;
        let (_, body) = check(&checker(&[&reg], &fleet)).await;
        assert_eq!(body["min_proxy_checked"], true);
        let update = &body["sniffers"][0]["update"];
        assert_eq!(update["compatible"], false);
        assert!(
            update["reason"].as_str().unwrap().contains("0.0.9"),
            "{update}"
        );
    }

    #[tokio::test]
    async fn check_groups_instances_by_installed_hash() {
        let (v1, v2) = (sha_of(b"v1"), sha_of(b"v2"));
        let (reg, _) =
            registry_with_versions(&[("0.2.0", "0.1", v2.clone()), ("0.1.0", "0.1", v1.clone())])
                .await;
        let fleet =
            fake_fleet(json!({ "a": listing(&v1), "b": listing(&v2), "c": listing(&v1) })).await;
        let (_, body) = check(&checker(&[&reg], &fleet)).await;
        let rows = body["sniffers"].as_array().unwrap();
        assert_eq!(rows.len(), 2, "a half-upgraded fleet is two rows: {body}");
        let old = rows
            .iter()
            .find(|r| r["installed_version"] == "0.1.0")
            .unwrap();
        let new = rows
            .iter()
            .find(|r| r["installed_version"] == "0.2.0")
            .unwrap();
        assert_eq!(old["instances"], json!(["a", "c"]));
        assert_eq!(new["instances"], json!(["b"]));
        assert!(new["update"].is_null(), "already on the newest");
    }

    #[tokio::test]
    async fn check_refetches_even_when_cached() {
        let old = sha_of(b"v1");
        let (reg, hits) = registry_with_versions(&[("0.1.0", "0.1", old.clone())]).await;
        let fleet = fake_fleet(json!({ "a": listing(&old) })).await;
        let h = checker(&[&reg], &fleet);
        let id = h.registry_id.clone();
        h.call("GET", &format!("/api/registries/{id}/sniffers"), None)
            .await;
        assert_eq!(*hits.lock().unwrap(), 1, "the listing fills the cache");
        check(&h).await;
        check(&h).await;
        assert_eq!(*hits.lock().unwrap(), 3, "every check goes to the registry");
    }

    #[tokio::test]
    async fn check_with_unreachable_registry_reports_it_and_still_answers_for_the_rest() {
        let old = sha_of(b"v1");
        let (good, _) = registry_with_versions(&[
            ("0.2.0", "0.1", sha_of(b"v2")),
            ("0.1.0", "0.1", old.clone()),
        ])
        .await;
        let fleet = fake_fleet(json!({ "a": listing(&old) })).await;
        let (status, body) = check(&checker(&["http://127.0.0.1:1", &good], &fleet)).await;
        assert_eq!(status, StatusCode::OK);
        let regs = body["registries"].as_array().unwrap();
        assert_eq!(
            regs.iter().filter(|r| r["ok"] == false).count(),
            1,
            "{body}"
        );
        assert!(regs
            .iter()
            .any(|r| r["ok"] == false && r["error"].is_string()));
        assert_eq!(body["sniffers"][0]["update"]["version"], "0.2.0");
    }

    #[tokio::test]
    async fn check_needs_the_operator_role() {
        let (reg, _) = registry_with_versions(&[("0.1.0", "0.1", sha_of(b"v1"))]).await;
        let fleet = fake_fleet(json!({})).await;
        let h = harness(&reg, &fleet, false, crate::role::Role::Viewer).await;
        let (status, _) = check(&h).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[test]
    fn instance_names_are_encoded_as_one_path_segment() {
        assert_eq!(path_segment("fra-1.eu_a~b"), "fra-1.eu_a~b");
        assert_eq!(path_segment("a/b?c#d%e f"), "a%2Fb%3Fc%23d%25e%20f");
        assert_eq!(path_segment("../x"), "..%2Fx");
    }
}
