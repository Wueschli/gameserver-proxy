//! The plugin admin API: upload a module, approve its capabilities, manage installs.
//!
//! The flow mirrors sniffer installs: `POST /plugins/modules` stores the bytes and returns
//! what the module declares (the operator reads it); `POST /plugins` names a stored module
//! and the capability set the operator approves, compiles it through the host's bounded
//! pool, and only then records the install. Every route sits behind the admin bearer layer.
//!
//! Under HA ([`PluginsState::ha`]) the installs and state are the replicated ones. Reads
//! (`GET /plugins`, `GET /plugins/{id}`) answer from this replica. Writes are proposed
//! through Raft (a follower forwards them to the leader, like the other write routes),
//! and a module upload and an install, which need the module bytes, and a status read,
//! which needs the tick results, run on the leader: a follower forwards the whole request.
//! An upload lands on the leader, which pushes the module to a quorum of members before it
//! proposes the install ([`super::peer`]); the other replicas fetch what they miss.

// The error side is an axum `Response`, which is what these handlers return either way.
#![allow(clippy::result_large_err)]

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{middleware, Json, Router};
use serde::{Deserialize, Serialize};
use wayhouse_plugin_host::{inspect, Capabilities, CompilePool, PoolError, MAX_MODULE_BYTES};

use super::runner::Runner;
use super::{valid_id, valid_name, InstallRecord, PluginStore, PluginStoreError, MAX_CONFIG_BYTES};
use crate::ha::client::{forward_to_current_leader, propose_write_as, ForwardHeaders};
use crate::ha::{HaHandle, PluginReject, WriteRequest, WriteResponse};

#[derive(Clone)]
pub struct PluginsState {
    pub store: PluginStore,
    pub pool: Arc<CompilePool>,
    /// Source of `/plugins/{id}/status`.
    pub runner: Arc<Runner>,
    /// Bearer token every `/plugins*` request must present, or `None` to leave it open.
    pub auth_token: Option<Arc<str>>,
    /// `Some` when the controller runs under HA: `store` is then the state machine's.
    pub ha: Option<Arc<HaHandle>>,
}

type ApiError = Response;

fn err(status: StatusCode, msg: impl std::fmt::Display) -> Response {
    (
        status,
        Json(serde_json::json!({ "error": msg.to_string() })),
    )
        .into_response()
}

#[allow(clippy::needless_pass_by_value)] // `map_err` callback
fn internal(e: PluginStoreError) -> ApiError {
    tracing::error!(error = %e, "plugin store failure");
    err(StatusCode::INTERNAL_SERVER_ERROR, "plugin store failure")
}

impl PluginsState {
    /// Under HA, on a replica that is not the leader: the leader's answer to this request,
    /// forwarded as it came. `None` when this node should handle it itself.
    async fn to_leader(
        &self,
        method: Method,
        path: &str,
        headers: &HeaderMap,
        body: Bytes,
    ) -> Option<Response> {
        let ha = self.ha.as_ref()?;
        if ha.is_leader() {
            return None;
        }
        let forward = ForwardHeaders::from_headers(headers);
        if forward.forwarded {
            return Some(err(
                StatusCode::SERVICE_UNAVAILABLE,
                "the leader changed while the request was forwarded; retry shortly",
            ));
        }
        Some(forward_to_current_leader(ha, method, path, body, &forward).await)
    }
}

pub fn router(state: PluginsState) -> Router {
    Router::new()
        .route(
            "/plugins/modules",
            post(upload_module).layer(DefaultBodyLimit::max(MAX_MODULE_BYTES)),
        )
        .route("/plugins", post(install).get(list))
        .route("/plugins/{id}", get(get_one).delete(remove))
        .route("/plugins/{id}/status", get(status))
        .route("/plugins/{id}/enable", post(enable))
        .route("/plugins/{id}/disable", post(disable))
        .route_layer(middleware::from_fn_with_state(
            wayhouse_http::server::BearerAuth::new(state.auth_token.as_deref()),
            wayhouse_http::server::require_bearer,
        ))
        .with_state(state)
}

/// The `/plugins` routes of a controller that does not serve plugins: 501 with a reason.
pub fn disabled_router(reason: &'static str) -> Router {
    let refuse = move || async move { err(StatusCode::NOT_IMPLEMENTED, reason) };
    Router::new()
        .route("/plugins", axum::routing::any(refuse))
        .route("/plugins/{*rest}", axum::routing::any(refuse))
}

#[derive(Serialize)]
struct UploadResponse {
    sha256: String,
    size: usize,
    abi: String,
    capabilities: Capabilities,
}

async fn upload_module(
    State(st): State<PluginsState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(forwarded) = st
        .to_leader(Method::POST, "/plugins/modules", &headers, body.clone())
        .await
    {
        return forwarded;
    }
    let info = match inspect(&body) {
        Ok(i) => i,
        Err(e) => return err(StatusCode::BAD_REQUEST, e),
    };
    if let Err(e) = st.store.put_blob(&info.sha256, &body) {
        return internal(e);
    }
    (
        StatusCode::CREATED,
        Json(UploadResponse {
            sha256: info.sha256,
            size: body.len(),
            abi: info.abi.to_string(),
            capabilities: info.caps,
        }),
    )
        .into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InstallRequest {
    name: String,
    sha256: String,
    /// What the operator approves; the module's declaration must be within it.
    approved: Capabilities,
    #[serde(default)]
    config: serde_json::Value,
    #[serde(default = "yes")]
    enabled: bool,
}

fn yes() -> bool {
    true
}

fn actor(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-actor")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.chars().take(128).collect())
}

fn pool_error(e: PoolError) -> ApiError {
    match e {
        PoolError::Module(m) => err(StatusCode::UNPROCESSABLE_ENTITY, m),
        PoolError::Busy | PoolError::TimedOut => err(
            StatusCode::SERVICE_UNAVAILABLE,
            "the plugin compiler is busy, retry shortly",
        ),
    }
}

async fn install(State(st): State<PluginsState>, headers: HeaderMap, body: Bytes) -> Response {
    match install_inner(&st, &headers, body).await {
        Ok(r) | Err(r) => r,
    }
}

/// `Err` carries an error answer or a forwarded one; both are just the response.
async fn install_inner(
    st: &PluginsState,
    headers: &HeaderMap,
    body: Bytes,
) -> Result<Response, Response> {
    if let Some(forwarded) = st
        .to_leader(Method::POST, "/plugins", headers, body.clone())
        .await
    {
        return Err(forwarded);
    }
    let req: InstallRequest = serde_json::from_slice(&body).map_err(|e| {
        err(
            if e.is_data() {
                StatusCode::UNPROCESSABLE_ENTITY
            } else {
                StatusCode::BAD_REQUEST
            },
            e,
        )
    })?;
    if !valid_name(&req.name) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "name must be 1 to 64 characters of a-z, 0-9 and '-', not starting with '-'",
        ));
    }
    let config = if req.config.is_null() {
        serde_json::json!({})
    } else {
        req.config
    };
    if serde_json::to_vec(&config).map_or(true, |b| b.len() > MAX_CONFIG_BYTES) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            format!("config is over {MAX_CONFIG_BYTES} bytes"),
        ));
    }
    // The approved set goes through the same validation as a declared one.
    let approved_json = serde_json::to_vec(&req.approved).expect("capabilities serialize");
    let approved = Capabilities::parse(&approved_json)
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("approved: {e}")))?;
    let Some(bytes) = st.store.get_blob(&req.sha256).map_err(internal)? else {
        return Err(err(
            StatusCode::NOT_FOUND,
            "no uploaded module with that sha256; POST /plugins/modules first",
        ));
    };
    let size = bytes.len();
    if let Some(ha) = &st.ha {
        // Before the compile: a refusal costs the operator nothing.
        super::peer::check_support(ha)
            .await
            .map_err(|e| err(StatusCode::SERVICE_UNAVAILABLE, e))?;
    }
    let for_peers = st.ha.is_some().then(|| Bytes::from(bytes.clone()));
    let pool = st.pool.clone();
    let approved_for_load = approved.clone();
    tokio::task::spawn_blocking(move || pool.load(bytes, approved_for_load))
        .await
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "compile task failed"))?
        .map_err(pool_error)?;
    let record = InstallRecord {
        id: format!("{:016x}", rand::random::<u64>()),
        name: req.name,
        sha256: req.sha256,
        size,
        approved,
        config,
        enabled: req.enabled,
        created_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
        created_by: actor(headers),
    };
    // Every replica checks this again when it applies the entry; checking here is what
    // gives the operator the reason.
    record
        .validate()
        .map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    if let (Some(ha), Some(bytes)) = (&st.ha, for_peers) {
        // The record is committed only once a quorum holds the module, so an install
        // cannot be stranded on a node that dies right after the upload.
        super::peer::replicate_to_quorum(ha, &record.sha256, bytes)
            .await
            .map_err(|e| err(StatusCode::SERVICE_UNAVAILABLE, e))?;
        // Proposed here and not forwarded: this request already ran on the leader, and a
        // second hop to a newer leader would not have the module checked or the upload.
        return match ha
            .raft
            .client_write(WriteRequest::PluginInstall(record.clone()))
            .await
        {
            Ok(done) => Ok(match done.data {
                WriteResponse::PluginApplied(_) => {
                    (StatusCode::CREATED, Json(record)).into_response()
                }
                other => unexpected(&other),
            }),
            Err(e) => {
                tracing::warn!(error = %e, "could not propose a plugin install");
                Err(err(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "leadership changed while the install was running; retry shortly",
                ))
            }
        };
    }
    st.store.create(&record).map_err(|e| match e {
        PluginStoreError::BlobMissing => err(
            StatusCode::CONFLICT,
            "the module was removed while installing; upload it again",
        ),
        e => internal(e),
    })?;
    Ok((StatusCode::CREATED, Json(record)).into_response())
}

/// The status and body of a plugin entry the state machine refused or answered oddly.
fn unexpected(resp: &WriteResponse) -> Response {
    match resp {
        WriteResponse::PluginRejected(PluginReject::NoSuchInstall) => {
            err(StatusCode::NOT_FOUND, "no such install")
        }
        WriteResponse::PluginRejected(PluginReject::Exists) => {
            err(StatusCode::CONFLICT, "an install with that id exists")
        }
        WriteResponse::PluginRejected(PluginReject::Invalid) => {
            err(StatusCode::BAD_REQUEST, "the plugin entry is malformed")
        }
        other => {
            tracing::error!(?other, "unexpected raft response to a plugin write");
            err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "unexpected raft response to a plugin write",
            )
        }
    }
}

async fn list(State(st): State<PluginsState>) -> Result<Json<Vec<InstallRecord>>, ApiError> {
    Ok(Json(st.store.list().map_err(internal)?))
}

async fn get_one(
    State(st): State<PluginsState>,
    Path(id): Path<String>,
) -> Result<Json<InstallRecord>, ApiError> {
    st.store
        .get(&id)
        .map_err(internal)?
        .map(Json)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "no such install"))
}

/// What the install's ticks have done (empty before the first one). Under HA only the
/// leader ticks, so a follower forwards the read.
async fn status(
    State(st): State<PluginsState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if !valid_id(&id) {
        return err(StatusCode::NOT_FOUND, "no such install");
    }
    let path = format!("/plugins/{id}/status");
    if let Some(forwarded) = st
        .to_leader(Method::GET, &path, &headers, Bytes::new())
        .await
    {
        return forwarded;
    }
    match st.store.get(&id) {
        Err(e) => internal(e),
        Ok(None) => err(StatusCode::NOT_FOUND, "no such install"),
        Ok(Some(_)) => Json(st.runner.status(&id).unwrap_or_default()).into_response(),
    }
}

async fn set_enabled(
    st: &PluginsState,
    headers: &HeaderMap,
    id: String,
    enabled: bool,
) -> Response {
    if !valid_id(&id) {
        return err(StatusCode::NOT_FOUND, "no such install");
    }
    if let Some(ha) = &st.ha {
        let path = format!(
            "/plugins/{id}/{}",
            if enabled { "enable" } else { "disable" }
        );
        let (store, read_id) = (st.store.clone(), id.clone());
        return propose_write_as(
            ha,
            WriteRequest::PluginSetEnabled { id, enabled },
            Method::POST,
            &path,
            String::new(),
            &ForwardHeaders::from_headers(headers),
            move |resp| match resp {
                WriteResponse::PluginApplied(_) => match store.get(&read_id) {
                    Ok(Some(record)) => Json(record).into_response(),
                    Ok(None) => err(StatusCode::NOT_FOUND, "no such install"),
                    Err(e) => internal(e),
                },
                other => unexpected(&other),
            },
        )
        .await;
    }
    match st.store.set_enabled(&id, enabled) {
        Ok(Some(record)) => Json(record).into_response(),
        Ok(None) => err(StatusCode::NOT_FOUND, "no such install"),
        Err(e) => internal(e),
    }
}

async fn enable(
    State(st): State<PluginsState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    set_enabled(&st, &headers, id, true).await
}

async fn disable(
    State(st): State<PluginsState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    set_enabled(&st, &headers, id, false).await
}

async fn remove(
    State(st): State<PluginsState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if !valid_id(&id) {
        return err(StatusCode::NOT_FOUND, "no such install");
    }
    if let Some(ha) = &st.ha {
        let path = format!("/plugins/{id}");
        return propose_write_as(
            ha,
            WriteRequest::PluginDelete { id },
            Method::DELETE,
            &path,
            String::new(),
            &ForwardHeaders::from_headers(&headers),
            |resp| match resp {
                WriteResponse::PluginApplied(_) => StatusCode::NO_CONTENT.into_response(),
                other => unexpected(&other),
            },
        )
        .await;
    }
    match st.store.delete(&id) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => err(StatusCode::NOT_FOUND, "no such install"),
        Err(e) => internal(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;
    use wayhouse_plugin_host::{Limits, PluginHost};

    fn module(caps: &str) -> Vec<u8> {
        let caps = caps.replace('"', "\\\"");
        wat::parse_str(format!(
            r#"(module
  (@custom "wayhouse.plugin-abi" "\00\00\01\00")
  (@custom "wayhouse.plugin-caps" "{caps}")
  (import "wayhouse" "log" (func $log (param i32 i32 i32)))
  (memory (export "memory") 1)
  (func (export "alloc") (param i32) (result i32) (i32.const 0))
  (func (export "init") (param i32 i32))
  (func (export "on_timer")))"#
        ))
        .unwrap()
    }

    const CAPS: &str = r#"{"triggers":{"on_timer":true},"tick_interval_secs":30,"log":true}"#;

    fn state(token: Option<&str>) -> (PluginsState, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = sled::open(dir.path()).unwrap();
        let host = Arc::new(PluginHost::new(Limits::default()).unwrap());
        let pool = Arc::new(CompilePool::new(host, 2, 4, Duration::from_secs(30)).unwrap());
        let store = PluginStore::open(&db).unwrap();
        (
            PluginsState {
                runner: Runner::new(store.clone(), pool.clone()),
                store,
                pool,
                auth_token: token.map(Arc::from),
                ha: None,
            },
            dir,
        )
    }

    async fn call(
        app: &Router,
        method: &str,
        uri: &str,
        body: Vec<u8>,
        token: Option<&str>,
    ) -> (StatusCode, serde_json::Value) {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json");
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        let resp = app
            .clone()
            .oneshot(req.body(Body::from(body)).unwrap())
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

    async fn upload(app: &Router, m: &[u8]) -> String {
        let (status, v) = call(app, "POST", "/plugins/modules", m.to_vec(), None).await;
        assert_eq!(status, StatusCode::CREATED, "{v}");
        v["sha256"].as_str().unwrap().to_string()
    }

    fn install_body(sha: &str, approved: &str) -> Vec<u8> {
        format!(r#"{{"name":"demo","sha256":"{sha}","approved":{approved},"config":{{"a":1}}}}"#)
            .into_bytes()
    }

    #[tokio::test]
    async fn upload_reports_what_the_module_declares() {
        let (st, _d) = state(None);
        let app = router(st);
        let m = module(CAPS);
        let (status, v) = call(&app, "POST", "/plugins/modules", m.clone(), None).await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(v["abi"], "0.1");
        assert_eq!(v["size"], m.len());
        assert_eq!(v["capabilities"]["log"], true);
        assert_eq!(v["capabilities"]["tick_interval_secs"], 30);
    }

    #[tokio::test]
    async fn status_is_404_for_an_unknown_install_and_empty_before_the_first_tick() {
        let (st, _d) = state(None);
        let app = router(st);
        let (status, _) = call(&app, "GET", "/plugins/nope/status", Vec::new(), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let sha = upload(&app, &module(CAPS)).await;
        let (_, rec) = call(&app, "POST", "/plugins", install_body(&sha, CAPS), None).await;
        let uri = format!("/plugins/{}/status", rec["id"].as_str().unwrap());
        let (status, v) = call(&app, "GET", &uri, Vec::new(), None).await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(v["ticks"], 0);
        assert!(v["last_ok"].is_null());
        assert_eq!(v["logs"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn a_bad_module_is_a_400_and_stores_nothing() {
        let (st, _d) = state(None);
        let app = router(st.clone());
        let (status, _) = call(&app, "POST", "/plugins/modules", b"junk".to_vec(), None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(st.store.list().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_oversize_body_is_refused() {
        let (st, _d) = state(None);
        let app = router(st);
        let big = vec![0u8; MAX_MODULE_BYTES + 1];
        let (status, _) = call(&app, "POST", "/plugins/modules", big, None).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn install_records_the_approval_and_lists_it() {
        let (st, _d) = state(None);
        let app = router(st);
        let sha = upload(&app, &module(CAPS)).await;
        let (status, rec) = call(&app, "POST", "/plugins", install_body(&sha, CAPS), None).await;
        assert_eq!(status, StatusCode::CREATED, "{rec}");
        assert_eq!(rec["sha256"], sha);
        assert_eq!(rec["enabled"], true);
        assert_eq!(rec["approved"]["log"], true);
        let id = rec["id"].as_str().unwrap();
        assert_eq!(id.len(), 16);
        let (_, listed) = call(&app, "GET", "/plugins", vec![], None).await;
        assert_eq!(listed.as_array().unwrap().len(), 1);
        let (status, one) = call(&app, "GET", &format!("/plugins/{id}"), vec![], None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(one["name"], "demo");
    }

    #[tokio::test]
    async fn approving_less_than_declared_is_refused_and_stores_nothing() {
        let (st, _d) = state(None);
        let app = router(st.clone());
        let sha = upload(&app, &module(CAPS)).await;
        let narrower = r#"{"triggers":{"on_timer":true},"tick_interval_secs":30}"#;
        let (status, v) = call(&app, "POST", "/plugins", install_body(&sha, narrower), None).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
        assert!(v["error"].as_str().unwrap().contains("log"));
        assert!(st.store.list().unwrap().is_empty());
    }

    #[tokio::test]
    async fn install_needs_an_uploaded_module_and_a_valid_name() {
        let (st, _d) = state(None);
        let app = router(st);
        let (status, _) = call(&app, "POST", "/plugins", install_body("nope", CAPS), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let sha = upload(&app, &module(CAPS)).await;
        let body = format!(r#"{{"name":"Bad Name","sha256":"{sha}","approved":{CAPS}}}"#);
        let (status, _) = call(&app, "POST", "/plugins", body.into_bytes(), None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn enable_disable_and_delete_manage_the_install() {
        let (st, _d) = state(None);
        let app = router(st.clone());
        let sha = upload(&app, &module(CAPS)).await;
        let (_, rec) = call(&app, "POST", "/plugins", install_body(&sha, CAPS), None).await;
        let id = rec["id"].as_str().unwrap().to_string();
        let (_, v) = call(
            &app,
            "POST",
            &format!("/plugins/{id}/disable"),
            vec![],
            None,
        )
        .await;
        assert_eq!(v["enabled"], false);
        let (_, v) = call(&app, "POST", &format!("/plugins/{id}/enable"), vec![], None).await;
        assert_eq!(v["enabled"], true);
        let (status, _) = call(&app, "DELETE", &format!("/plugins/{id}"), vec![], None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(st.store.get_blob(&sha).unwrap().is_none());
        let (status, _) = call(&app, "GET", &format!("/plugins/{id}"), vec![], None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = call(&app, "DELETE", &format!("/plugins/{id}"), vec![], None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn two_installs_share_one_blob() {
        let (st, _d) = state(None);
        let app = router(st.clone());
        let sha = upload(&app, &module(CAPS)).await;
        let sha2 = upload(&app, &module(CAPS)).await;
        assert_eq!(sha, sha2);
        let (_, a) = call(&app, "POST", "/plugins", install_body(&sha, CAPS), None).await;
        let (_, b) = call(&app, "POST", "/plugins", install_body(&sha, CAPS), None).await;
        assert_ne!(a["id"], b["id"]);
        let id_a = a["id"].as_str().unwrap();
        call(&app, "DELETE", &format!("/plugins/{id_a}"), vec![], None).await;
        assert!(st.store.get_blob(&sha).unwrap().is_some());
    }

    #[tokio::test]
    async fn a_full_compile_queue_answers_503() {
        let dir = tempfile::tempdir().unwrap();
        let db = sled::open(dir.path()).unwrap();
        let host = Arc::new(PluginHost::new(Limits::default()).unwrap());
        // A zero timeout times every compile out.
        let pool = Arc::new(CompilePool::new(host, 1, 1, Duration::ZERO).unwrap());
        let store = PluginStore::open(&db).unwrap();
        let st = PluginsState {
            runner: Runner::new(store.clone(), pool.clone()),
            store,
            pool,
            auth_token: None,
            ha: None,
        };
        let app = router(st);
        let sha = upload(&app, &module(CAPS)).await;
        let (status, _) = call(&app, "POST", "/plugins", install_body(&sha, CAPS), None).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn the_bearer_token_is_required_when_one_is_set() {
        let (st, _d) = state(Some("sekrit-sekrit-sekrit"));
        let app = router(st);
        let (status, _) = call(&app, "GET", "/plugins", vec![], None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) = call(
            &app,
            "GET",
            "/plugins",
            vec![],
            Some("sekrit-sekrit-sekrit"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn the_disabled_router_answers_501_with_the_reason() {
        let app = disabled_router("plugins are off");
        let (status, v) = call(&app, "GET", "/plugins", vec![], None).await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
        assert_eq!(v["error"], "plugins are off");
        let (status, _) = call(&app, "POST", "/plugins/x/enable", vec![], None).await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
    }

    // ---- under HA (a single-node Raft group that leads itself) ----

    async fn ha_state() -> (PluginsState, tempfile::TempDir) {
        let (handle, _cluster, store, dir) =
            crate::ha::test_support::single_node_with_plugins(1, "127.0.0.1:1").await;
        let host = Arc::new(PluginHost::new(Limits::default()).unwrap());
        let pool = Arc::new(CompilePool::new(host, 2, 4, Duration::from_secs(30)).unwrap());
        (
            PluginsState {
                runner: Runner::new_ha(store.clone(), pool.clone(), handle.clone()),
                store,
                pool,
                auth_token: None,
                ha: Some(handle),
            },
            dir,
        )
    }

    #[tokio::test]
    async fn under_ha_install_toggle_and_delete_go_through_the_replicated_state() {
        let (st, _d) = ha_state().await;
        let app = router(st.clone());
        let sha = upload(&app, &module(CAPS)).await;
        let (status, rec) = call(&app, "POST", "/plugins", install_body(&sha, CAPS), None).await;
        assert_eq!(status, StatusCode::CREATED, "{rec}");
        let id = rec["id"].as_str().unwrap().to_string();
        // The record is in the state machine's store, put there by an applied entry.
        assert_eq!(st.store.get(&id).unwrap().unwrap().name, "demo");
        assert!(st.store.applied_index().unwrap().is_some());

        let (status, v) = call(
            &app,
            "POST",
            &format!("/plugins/{id}/disable"),
            vec![],
            None,
        )
        .await;
        assert_eq!(
            (status, &v["enabled"]),
            (StatusCode::OK, &serde_json::json!(false))
        );
        let (_, v) = call(&app, "POST", &format!("/plugins/{id}/enable"), vec![], None).await;
        assert_eq!(v["enabled"], true);

        let (status, _) = call(&app, "DELETE", &format!("/plugins/{id}"), vec![], None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(st.store.get(&id).unwrap().is_none());
        let (status, _) = call(&app, "DELETE", &format!("/plugins/{id}"), vec![], None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = call(
            &app,
            "POST",
            "/plugins/00000000000000ff/enable",
            vec![],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn under_ha_the_install_checks_still_run_on_the_leader() {
        let (st, _d) = ha_state().await;
        let app = router(st.clone());
        let sha = upload(&app, &module(CAPS)).await;
        let narrower = r#"{"triggers":{"on_timer":true},"tick_interval_secs":30}"#;
        let (status, _) = call(&app, "POST", "/plugins", install_body(&sha, narrower), None).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        let missing = install_body(&"0".repeat(64), CAPS);
        let (status, _) = call(&app, "POST", "/plugins", missing, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = call(&app, "POST", "/plugins", b"{".to_vec(), None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(st.store.list().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_malformed_body_is_422_when_the_json_is_fine_and_400_when_it_is_not() {
        let (st, _d) = state(None);
        let app = router(st);
        let (status, _) = call(&app, "POST", "/plugins", br#"{"nope":1}"#.to_vec(), None).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        let (status, _) = call(&app, "POST", "/plugins", b"{".to_vec(), None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn an_id_that_is_not_an_install_id_is_404_before_anything_is_forwarded() {
        let (st, _d) = ha_state().await;
        let app = router(st);
        for id in ["..%2Fconfig", "nope", "00000000000000FF"] {
            for (m, suffix) in [("GET", "/status"), ("POST", "/enable"), ("DELETE", "")] {
                let (status, _) =
                    call(&app, m, &format!("/plugins/{id}{suffix}"), vec![], None).await;
                assert_eq!(status, StatusCode::NOT_FOUND, "{m} {id}{suffix}");
            }
        }
    }
}
