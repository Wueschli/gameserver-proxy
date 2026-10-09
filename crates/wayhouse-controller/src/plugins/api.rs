//! The plugin admin API: upload a module, approve its capabilities, manage installs.
//!
//! The flow mirrors sniffer installs: `POST /plugins/modules` stores the bytes and returns
//! what the module declares (the operator reads it); `POST /plugins` names a stored module
//! and the capability set the operator approves, compiles it through the host's bounded
//! pool, and only then records the install. Every route sits behind the admin bearer layer.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{middleware, Json, Router};
use serde::{Deserialize, Serialize};
use wayhouse_plugin_host::{
    inspect, Capabilities, CompilePool, ModuleError, PoolError, MAX_MODULE_BYTES,
};

use super::runner::Runner;
use super::{valid_name, InstallRecord, PluginStore, PluginStoreError, MAX_CONFIG_BYTES};

#[derive(Clone)]
pub struct PluginsState {
    pub store: PluginStore,
    pub pool: Arc<CompilePool>,
    /// Source of `/plugins/{id}/status`.
    pub runner: Arc<Runner>,
    /// Bearer token every `/plugins*` request must present, or `None` to leave it open.
    pub auth_token: Option<Arc<str>>,
}

type ApiError = (StatusCode, Json<serde_json::Value>);

fn err(status: StatusCode, msg: impl std::fmt::Display) -> ApiError {
    (
        status,
        Json(serde_json::json!({ "error": msg.to_string() })),
    )
}

#[allow(clippy::needless_pass_by_value)] // `map_err` callback
fn internal(e: PluginStoreError) -> ApiError {
    tracing::error!(error = %e, "plugin store failure");
    err(StatusCode::INTERNAL_SERVER_ERROR, "plugin store failure")
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
    body: Bytes,
) -> Result<(StatusCode, Json<UploadResponse>), ApiError> {
    let info = inspect(&body).map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    st.store.put_blob(&info.sha256, &body).map_err(internal)?;
    Ok((
        StatusCode::CREATED,
        Json(UploadResponse {
            sha256: info.sha256,
            size: body.len(),
            abi: info.abi.to_string(),
            capabilities: info.caps,
        }),
    ))
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
        PoolError::Module(m @ ModuleError::CapsNotApproved(_)) => {
            err(StatusCode::UNPROCESSABLE_ENTITY, m)
        }
        PoolError::Module(m) => err(StatusCode::UNPROCESSABLE_ENTITY, m),
        PoolError::Busy | PoolError::TimedOut => err(
            StatusCode::SERVICE_UNAVAILABLE,
            "the plugin compiler is busy, retry shortly",
        ),
    }
}

async fn install(
    State(st): State<PluginsState>,
    headers: HeaderMap,
    Json(req): Json<InstallRequest>,
) -> Result<(StatusCode, Json<InstallRecord>), ApiError> {
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
        created_by: actor(&headers),
    };
    st.store.create(&record).map_err(|e| match e {
        PluginStoreError::BlobMissing => err(
            StatusCode::CONFLICT,
            "the module was removed while installing; upload it again",
        ),
        e => internal(e),
    })?;
    Ok((StatusCode::CREATED, Json(record)))
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

/// What the install's ticks have done (empty before the first one).
async fn status(
    State(st): State<PluginsState>,
    Path(id): Path<String>,
) -> Result<Json<super::runner::PluginStatus>, ApiError> {
    if st.store.get(&id).map_err(internal)?.is_none() {
        return Err(err(StatusCode::NOT_FOUND, "no such install"));
    }
    Ok(Json(st.runner.status(&id).unwrap_or_default()))
}

async fn set_enabled(
    st: &PluginsState,
    id: &str,
    enabled: bool,
) -> Result<Json<InstallRecord>, ApiError> {
    st.store
        .set_enabled(id, enabled)
        .map_err(internal)?
        .map(Json)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "no such install"))
}

async fn enable(
    State(st): State<PluginsState>,
    Path(id): Path<String>,
) -> Result<Json<InstallRecord>, ApiError> {
    set_enabled(&st, &id, true).await
}

async fn disable(
    State(st): State<PluginsState>,
    Path(id): Path<String>,
) -> Result<Json<InstallRecord>, ApiError> {
    set_enabled(&st, &id, false).await
}

async fn remove(
    State(st): State<PluginsState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    if st.store.delete(&id).map_err(internal)? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(err(StatusCode::NOT_FOUND, "no such install"))
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
}
