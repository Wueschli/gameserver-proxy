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
use axum::routing::{get, post, put};
use axum::{middleware, Json, Router};
use serde::{Deserialize, Serialize};
use wayhouse_plugin_host::{inspect, Capabilities, CompilePool, PoolError, MAX_MODULE_BYTES};

use super::runner::Runner;
use super::secrets::{KeyringHandle, OpenError, SealError, Sealed};
use super::{
    valid_id, valid_name, Applied, InstallRecord, PluginStore, PluginStoreError, MAX_CONFIG_BYTES,
};
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
    /// The node's secret keyring (empty without a key).
    pub keyring: KeyringHandle,
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
pub(crate) fn internal(e: PluginStoreError) -> ApiError {
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
    // Component-facing: the proxies' stream of plugin routes.
    let component = wayhouse_http::protocol::gate(
        Router::new().route("/plugin-routes/subscribe", get(super::routes::subscribe)),
        "controller",
    );
    Router::new()
        .merge(component)
        .route(
            "/plugins/modules",
            post(upload_module).layer(DefaultBodyLimit::max(MAX_MODULE_BYTES)),
        )
        .route("/plugins", post(install).get(list))
        .route("/plugins/{id}", get(get_one).delete(remove))
        .route("/plugins/{id}/status", get(status))
        .route(
            "/plugins/{id}/webhook",
            post(enable_webhook).delete(revoke_webhook),
        )
        .route("/plugins/{id}/enable", post(enable))
        .route("/plugins/{id}/disable", post(disable))
        .route("/plugins/{id}/secrets", get(list_secrets))
        .route(
            "/plugins/{id}/secrets/{slot}",
            put(put_secret)
                .delete(delete_secret)
                .layer(DefaultBodyLimit::max(16 * 1024)),
        )
        .route("/plugin-routes", get(super::routes::show))
        .route("/admin/plugins/secrets/rewrap", post(rewrap_secrets))
        .route("/admin/plugins/secrets/keys", get(secret_keys))
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
        .route("/plugin-routes", axum::routing::any(refuse))
        .route("/plugin-routes/subscribe", axum::routing::any(refuse))
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
        webhook: None,
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

// ---- webhooks ----

/// Turns the install's webhook on with a fresh token (enable and rotate are the same call).
/// The token is in this answer only; the store keeps its hash.
async fn enable_webhook(
    State(st): State<PluginsState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let record = match st.store.get(&id) {
        Ok(Some(r)) if valid_id(&id) => r,
        Ok(_) => return err(StatusCode::NOT_FOUND, "no such install"),
        Err(e) => return internal(e),
    };
    if !record.approved.triggers.on_webhook {
        return err(
            StatusCode::BAD_REQUEST,
            "the install did not approve the on_webhook trigger",
        );
    }
    let token = super::hooks::new_token();
    let hash = Some(super::hooks::hash_token(&token));
    let rejected = match &st.ha {
        Some(ha) => {
            if let Err(why) = super::peer::check_support(ha).await {
                return err(StatusCode::CONFLICT, why);
            }
            let path = format!("/plugins/{id}/webhook");
            let (shown, hook_path) = (token.clone(), format!("/plugins/{id}/hook"));
            // A follower's proposal is forwarded and the leader mints its own token, so only
            // the node that appends the entry ever shows a token that matches it.
            return propose_write_as(
                ha,
                WriteRequest::PluginSetWebhook {
                    id,
                    token_hash: hash,
                },
                Method::POST,
                &path,
                String::new(),
                &ForwardHeaders::from_headers(&headers),
                move |resp| match resp {
                    WriteResponse::PluginApplied(_) => {
                        Json(serde_json::json!({ "token": shown, "path": hook_path }))
                            .into_response()
                    }
                    WriteResponse::PluginRejected(PluginReject::NoSuchInstall) => {
                        err(StatusCode::NOT_FOUND, "no such install")
                    }
                    WriteResponse::PluginRejected(PluginReject::Invalid) => err(
                        StatusCode::BAD_REQUEST,
                        "the install did not approve the on_webhook trigger",
                    ),
                    other => unexpected(&other),
                },
            )
            .await;
        }
        None => st.store.set_webhook(&id, hash.as_deref()),
    };
    match rejected {
        Ok(Applied::Done(_)) => {
            tracing::info!(
                target: "wayhouse_controller::plugins::audit",
                what = "webhook enabled", install = %id, actor = actor(&headers).as_deref(),
                "plugin webhook changed"
            );
            Json(serde_json::json!({ "token": token, "path": format!("/plugins/{id}/hook") }))
                .into_response()
        }
        Ok(Applied::NoSuchInstall) => err(StatusCode::NOT_FOUND, "no such install"),
        Ok(_) => err(StatusCode::BAD_REQUEST, "the webhook was refused"),
        Err(e) => internal(e),
    }
}

/// Turns the install's webhook off; the old token stops working at once.
async fn revoke_webhook(
    State(st): State<PluginsState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if !valid_id(&id) {
        return err(StatusCode::NOT_FOUND, "no such install");
    }
    if let Some(ha) = &st.ha {
        let path = format!("/plugins/{id}/webhook");
        return propose_write_as(
            ha,
            WriteRequest::PluginSetWebhook {
                id,
                token_hash: None,
            },
            Method::DELETE,
            &path,
            String::new(),
            &ForwardHeaders::from_headers(&headers),
            |resp| match resp {
                WriteResponse::PluginApplied(_) => StatusCode::NO_CONTENT.into_response(),
                WriteResponse::PluginRejected(PluginReject::NoSuchInstall) => {
                    err(StatusCode::NOT_FOUND, "no such install")
                }
                other => unexpected(&other),
            },
        )
        .await;
    }
    match st.store.set_webhook(&id, None) {
        Ok(Applied::Done(_)) => {
            tracing::info!(
                target: "wayhouse_controller::plugins::audit",
                what = "webhook revoked", install = %id, actor = actor(&headers).as_deref(),
                "plugin webhook changed"
            );
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(Applied::NoSuchInstall) => err(StatusCode::NOT_FOUND, "no such install"),
        Ok(_) => err(StatusCode::BAD_REQUEST, "the webhook was refused"),
        Err(e) => internal(e),
    }
}

// ---- secrets (design: 2026-10-08-plugin-secret-storage-design.md) ----

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The install and its approved slot, or the `404` that says which is missing.
fn approved_slot(st: &PluginsState, id: &str, slot: &str) -> Result<InstallRecord, ApiError> {
    if !valid_id(id) || !wayhouse_plugin_host::caps::valid_slot_name(slot) {
        return Err(err(StatusCode::NOT_FOUND, "no such install or secret slot"));
    }
    let record = st
        .store
        .get(id)
        .map_err(internal)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "no such install"))?;
    if !record.approved.secrets.iter().any(|s| s.name == slot) {
        return Err(err(
            StatusCode::NOT_FOUND,
            "the install did not approve that secret slot",
        ));
    }
    Ok(record)
}

/// `{"value": "..."}`. Deliberately no `Debug`, and a parse failure never echoes the body.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SecretBody {
    value: String,
}

/// Maps a secret write's Raft answer to a response.
fn secret_answer(resp: &WriteResponse) -> Result<(), ApiError> {
    match resp {
        WriteResponse::PluginApplied(_) => Ok(()),
        WriteResponse::PluginRejected(PluginReject::NoSuchInstall) => {
            Err(err(StatusCode::NOT_FOUND, "no such install"))
        }
        WriteResponse::PluginRejected(PluginReject::Invalid) => Err(err(
            StatusCode::BAD_REQUEST,
            "the secret was refused (slot not approved or malformed)",
        )),
        other => Err(unexpected(other)),
    }
}

async fn put_secret(
    State(st): State<PluginsState>,
    headers: HeaderMap,
    Path((id, slot)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    if let Err(r) = approved_slot(&st, &id, &slot) {
        return r;
    }
    let Ok(parsed) = serde_json::from_slice::<SecretBody>(&body) else {
        // Never the parser's message: it can quote the submitted text.
        return err(
            StatusCode::BAD_REQUEST,
            "the body must be {\"value\": \"...\"}",
        );
    };
    let value = zeroize::Zeroizing::new(parsed.value.into_bytes());
    let sealed = match st.keyring.current().seal(&id, &slot, &value) {
        Ok(s) => s,
        Err(SealError::NoKey) => {
            return err(
                StatusCode::CONFLICT,
                "no secret key configured on this controller (--plugin-secret-key-file)",
            )
        }
        Err(SealError::Size) => {
            return err(
                StatusCode::BAD_REQUEST,
                format!(
                    "a secret must be {} to {} bytes",
                    wayhouse_plugin_host::secret::MIN_SECRET_BYTES,
                    super::secrets::MAX_SECRET_BYTES
                ),
            )
        }
    };
    drop(value);
    let (updated_at, actor) = (now(), actor(&headers));
    let result = match &st.ha {
        Some(ha) => {
            let entry = WriteRequest::PluginSecretSet {
                id: id.clone(),
                slot: slot.clone(),
                sealed: sealed.clone(),
                updated_at,
                actor,
            };
            match super::peer::propose_ciphertext(ha, entry).await {
                Ok(resp) => secret_answer(&resp),
                Err(why) => Err(err(StatusCode::SERVICE_UNAVAILABLE, why)),
            }
        }
        None => match st.store.put_secret(&id, &slot, &sealed, updated_at) {
            Ok(Applied::Done(_)) => {
                tracing::info!(
                    target: "wayhouse_controller::plugins::audit",
                    what = "set", install = %id, slot = %slot, key_id = %sealed.key_id,
                    actor = actor.as_deref(), "plugin secret changed"
                );
                Ok(())
            }
            Ok(Applied::NoSuchInstall) => Err(err(StatusCode::NOT_FOUND, "no such install")),
            Ok(_) => Err(err(StatusCode::BAD_REQUEST, "the secret was refused")),
            Err(e) => Err(internal(e)),
        },
    };
    match result {
        Ok(()) => Json(serde_json::json!({
            "slot": slot, "set": true, "key_id": sealed.key_id, "updated_at": updated_at
        }))
        .into_response(),
        Err(r) => r,
    }
}

async fn delete_secret(
    State(st): State<PluginsState>,
    headers: HeaderMap,
    Path((id, slot)): Path<(String, String)>,
) -> Response {
    if let Err(r) = approved_slot(&st, &id, &slot) {
        return r;
    }
    if let Some(ha) = &st.ha {
        let path = format!("/plugins/{id}/secrets/{slot}");
        let ha_for_purge = ha.clone();
        return propose_write_as(
            ha,
            WriteRequest::PluginSecretDelete {
                id,
                slot,
                actor: actor(&headers),
            },
            Method::DELETE,
            &path,
            String::new(),
            &ForwardHeaders::from_headers(&headers),
            move |resp| match secret_answer(&resp) {
                Ok(()) => {
                    // This runs where the entry was proposed: the leader.
                    super::peer::request_purge(&ha_for_purge);
                    StatusCode::NO_CONTENT.into_response()
                }
                Err(r) => r,
            },
        )
        .await;
    }
    match st.store.delete_secret(&id, &slot) {
        Ok(true) => {
            tracing::info!(
                target: "wayhouse_controller::plugins::audit",
                what = "delete", install = %id, slot = %slot, actor = actor(&headers).as_deref(),
                "plugin secret changed"
            );
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => err(StatusCode::NOT_FOUND, "no such install"),
        Err(e) => internal(e),
    }
}

/// Why a secret that is set cannot be used on this node, or `None` when it can.
pub(crate) fn unreadable(
    keyring: &KeyringHandle,
    id: &str,
    slot: &str,
    sealed: &Sealed,
) -> Option<String> {
    let ring = keyring.current();
    if ring.is_empty() {
        return Some("held: no secret key".into());
    }
    match ring.open(id, slot, sealed) {
        Ok(_) => None,
        Err(OpenError::KeyMissing(k)) => Some(format!("held: key {k} missing")),
        Err(_) => Some("held: the stored secret does not authenticate".into()),
    }
}

/// Per approved slot: whether it is set, under which key, and whether this node can use it.
/// Never a value.
async fn list_secrets(
    State(st): State<PluginsState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !valid_id(&id) {
        return Err(err(StatusCode::NOT_FOUND, "no such install"));
    }
    let record = st
        .store
        .get(&id)
        .map_err(internal)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "no such install"))?;
    let mut slots = Vec::new();
    for slot in &record.approved.secrets {
        let stored = st.store.get_secret(&id, &slot.name).map_err(internal)?;
        let status = match &stored {
            None => "unset".to_string(),
            Some(s) => {
                unreadable(&st.keyring, &id, &slot.name, &s.sealed).unwrap_or_else(|| "ok".into())
            }
        };
        slots.push(serde_json::json!({
            "slot": slot.name,
            "description": slot.description,
            "bound_hosts": slot.hosts,
            "set": stored.is_some(),
            "key_id": stored.as_ref().map(|s| s.sealed.key_id.clone()),
            "updated_at": stored.as_ref().map(|s| s.updated_at),
            "status": status,
        }));
    }
    Ok(Json(serde_json::json!({ "secrets": slots })))
}

/// Re-encrypts every secret under the active key (step 2 of a rotation). Runs on the
/// leader; refuses while a member lacks the active key.
async fn rewrap_secrets(State(st): State<PluginsState>, headers: HeaderMap) -> Response {
    if let Some(forwarded) = st
        .to_leader(
            Method::POST,
            "/admin/plugins/secrets/rewrap",
            &headers,
            Bytes::new(),
        )
        .await
    {
        return forwarded;
    }
    let ring = st.keyring.current();
    let Some(active) = ring.active_id().map(str::to_string) else {
        return err(
            StatusCode::CONFLICT,
            "no secret key configured on this controller",
        );
    };
    if let Some(ha) = &st.ha {
        if let Err(why) = super::peer::check_active_key(ha, &active).await {
            return err(
                StatusCode::CONFLICT,
                format!("every node needs the active key {active} first: {why}"),
            );
        }
    }
    let stored = match st.store.all_secrets() {
        Ok(a) => a,
        Err(e) => return internal(e),
    };
    let (mut items, mut unreadable) = (Vec::new(), Vec::new());
    for s in stored
        .into_iter()
        .filter(|s| s.stored.sealed.key_id != active)
    {
        let label = format!("{}/{}", s.id, s.slot);
        match ring.open(&s.id, &s.slot, &s.stored.sealed) {
            Ok(value) => match ring.seal(&s.id, &s.slot, value.expose()) {
                Ok(sealed) => items.push(super::RewrapItem {
                    id: s.id,
                    slot: s.slot,
                    from_nonce: s.stored.sealed.nonce,
                    sealed,
                }),
                Err(_) => unreadable.push(label),
            },
            Err(_) => unreadable.push(label),
        }
    }
    let total = items.len();
    let skipped = if items.is_empty() {
        Vec::new()
    } else {
        let updated_at = now();
        match &st.ha {
            Some(ha) => {
                let entry = WriteRequest::PluginSecretRewrap { items, updated_at };
                match super::peer::propose_ciphertext(ha, entry).await {
                    Ok(WriteResponse::PluginRewrapped(skipped)) => {
                        super::peer::request_purge(ha);
                        skipped
                    }
                    Ok(other) => return unexpected(&other),
                    Err(why) => return err(StatusCode::SERVICE_UNAVAILABLE, why),
                }
            }
            None => match st.store.rewrap(&items, updated_at) {
                Ok(skipped) => skipped,
                Err(e) => return internal(e),
            },
        }
    };
    Json(serde_json::json!({
        "rewrapped": total - skipped.len(),
        "skipped": skipped,
        "unreadable": unreadable,
        "active_key_id": active,
    }))
    .into_response()
}

/// The key ids this node holds and, under HA, each member's (never key material).
async fn secret_keys(State(st): State<PluginsState>) -> Json<serde_json::Value> {
    let mut nodes = serde_json::Map::new();
    if let Some(ha) = &st.ha {
        nodes.insert(
            ha.node_id.to_string(),
            serde_json::json!(st.keyring.current().key_ids()),
        );
        for (id, who) in super::peer::whoami_all(ha).await {
            nodes.insert(
                id.to_string(),
                match who {
                    Ok(w) => serde_json::json!(w.secret_key_ids),
                    Err(why) => serde_json::json!({ "error": why }),
                },
            );
        }
    }
    Json(serde_json::json!({
        "key_ids": st.keyring.current().key_ids(),
        "active": st.keyring.current().active_id(),
        "nodes": nodes,
    }))
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
                keyring: KeyringHandle::default(),
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

    const HOOK_CAPS: &str =
        r#"{"triggers":{"on_timer":true,"on_webhook":true},"tick_interval_secs":30,"log":true}"#;

    /// `on_webhook` answers `202` with an `x-run` header and the body `ok`.
    fn hook_module() -> Vec<u8> {
        let caps = HOOK_CAPS.replace('"', "\\\"");
        let resp = r#"{"status":202,"headers":{"X-Run":"1"},"body":"b2s="}"#;
        let (len, resp) = (resp.len(), resp.replace('"', "\\\""));
        wat::parse_str(format!(
            r#"(module
  (@custom "wayhouse.plugin-abi" "\00\00\01\00")
  (@custom "wayhouse.plugin-caps" "{caps}")
  (import "wayhouse" "log" (func $log (param i32 i32 i32)))
  (import "wayhouse" "webhook_respond" (func $respond (param i32 i32) (result i32)))
  (memory (export "memory") 2)
  (data (i32.const 60000) "{resp}")
  (global $bump (mut i32) (i32.const 1024))
  (func (export "alloc") (param i32) (result i32) (local $p i32)
    (local.set $p (global.get $bump))
    (global.set $bump (i32.add (global.get $bump) (local.get 0)))
    (local.get $p))
  (func (export "init") (param i32 i32))
  (func (export "on_timer"))
  (func (export "on_webhook") (param i32 i32)
    (drop (call $respond (i32.const 60000) (i32.const {len})))))"#
        ))
        .unwrap()
    }

    async fn hit(
        app: &Router,
        uri: &str,
        token: Option<&str>,
        body: Vec<u8>,
    ) -> (StatusCode, axum::http::HeaderMap) {
        let mut req = Request::builder().method("POST").uri(uri);
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        let mut req = req.body(Body::from(body)).unwrap();
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo(wayhouse_http::tls::PeerAddr(
                "203.0.113.9:4000".parse().unwrap(),
            )));
        let resp = app.clone().oneshot(req).await.unwrap();
        (resp.status(), resp.headers().clone())
    }

    #[tokio::test]
    async fn a_webhook_is_enabled_with_a_token_shown_once_and_guarded_by_it() {
        let (st, _d) = state(None);
        let app = router(st.clone());
        // An install that never approved on_webhook cannot enable one.
        let sha = upload(&app, &module(CAPS)).await;
        let (_, plain) = call(&app, "POST", "/plugins", install_body(&sha, CAPS), None).await;
        let plain_id = plain["id"].as_str().unwrap();
        let (status, _) = call(
            &app,
            "POST",
            &format!("/plugins/{plain_id}/webhook"),
            vec![],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let sha = upload(&app, &hook_module()).await;
        let (_, rec) = call(
            &app,
            "POST",
            "/plugins",
            install_body(&sha, HOOK_CAPS),
            None,
        )
        .await;
        let id = rec["id"].as_str().unwrap().to_string();
        let (status, shown) = call(
            &app,
            "POST",
            &format!("/plugins/{id}/webhook"),
            vec![],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{shown}");
        let token = shown["token"].as_str().unwrap().to_string();
        let path = shown["path"].as_str().unwrap().to_string();
        assert_eq!(path, format!("/plugins/{id}/hook"));
        // The record keeps the hash, never the token; the listing does not show it either.
        let stored = serde_json::to_string(&st.store.get(&id).unwrap().unwrap()).unwrap();
        assert!(!stored.contains(&token));
        let (_, listed) = call(&app, "GET", "/plugins", vec![], None).await;
        assert!(!listed.to_string().contains(&token));

        let hooks = super::super::hooks::router(super::super::hooks::HookState::new(
            st.store.clone(),
            st.runner.clone(),
            None,
        ));
        let (ok, headers) = hit(&hooks, &path, Some(&token), b"{}".to_vec()).await;
        assert_eq!(ok, StatusCode::ACCEPTED);
        assert_eq!(headers["x-run"], "1");
        // Missing, wrong and unknown-install tokens all look the same.
        for (uri, tok) in [
            (path.as_str(), None),
            (path.as_str(), Some("whk_wrong")),
            ("/plugins/p-unknown/hook", Some(token.as_str())),
        ] {
            assert_eq!(
                hit(&hooks, uri, tok, vec![]).await.0,
                StatusCode::UNAUTHORIZED
            );
        }
        // Over the body cap: 413, before the guest runs.
        let big = vec![0u8; wayhouse_plugin_host::webhook::MAX_WEBHOOK_BODY + 1];
        assert_eq!(
            hit(&hooks, &path, Some(&token), big).await.0,
            StatusCode::PAYLOAD_TOO_LARGE
        );

        // Rotating swaps the token; revoking turns the webhook off.
        let (_, again) = call(
            &app,
            "POST",
            &format!("/plugins/{id}/webhook"),
            vec![],
            None,
        )
        .await;
        let rotated = again["token"].as_str().unwrap().to_string();
        assert_ne!(rotated, token);
        assert_eq!(
            hit(&hooks, &path, Some(&token), vec![]).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            hit(&hooks, &path, Some(&rotated), vec![]).await.0,
            StatusCode::ACCEPTED
        );
        let (status, _) = call(
            &app,
            "DELETE",
            &format!("/plugins/{id}/webhook"),
            vec![],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(
            hit(&hooks, &path, Some(&rotated), vec![]).await.0,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn one_source_is_rate_limited_before_any_token_is_compared() {
        let (st, _d) = state(None);
        let hooks = super::super::hooks::router(super::super::hooks::HookState::new(
            st.store.clone(),
            st.runner.clone(),
            None,
        ));
        let mut seen = Vec::new();
        for _ in 0..30 {
            seen.push(
                hit(&hooks, "/plugins/p-x/hook", Some("whk_guess"), vec![])
                    .await
                    .0,
            );
        }
        assert_eq!(seen[0], StatusCode::UNAUTHORIZED);
        assert!(seen.contains(&StatusCode::TOO_MANY_REQUESTS), "{seen:?}");
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
            keyring: KeyringHandle::default(),
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
                keyring: KeyringHandle::default(),
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
    const SECRET_CAPS: &str = r#"{"triggers":{"on_timer":true},"tick_interval_secs":30,"log":true,"http":{"hosts":[{"host":"panel.example","port":8443}]},"secrets":[{"name":"PANEL_TOKEN","hosts":["panel.example"]}]}"#;
    const KEY_A: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
    const SENTINEL: &str = "sentinel-secret-value-9f3a1c77";

    fn with_key(st: &mut PluginsState, text: &str) {
        st.keyring = KeyringHandle::new(super::super::secrets::Keyring::parse(text).unwrap());
    }

    async fn installed(app: &Router) -> String {
        let sha = upload(app, &module(SECRET_CAPS)).await;
        let (status, rec) = call(
            app,
            "POST",
            "/plugins",
            install_body(&sha, SECRET_CAPS),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{rec}");
        rec["id"].as_str().unwrap().to_string()
    }

    fn put(v: &str) -> Vec<u8> {
        serde_json::json!({ "value": v }).to_string().into_bytes()
    }

    #[tokio::test]
    async fn a_secret_needs_a_key_an_approved_slot_and_a_sane_size() {
        let (mut st, _d) = state(None);
        let store = st.store.clone();
        let app = router(st.clone());
        let id = installed(&app).await;
        let uri = format!("/plugins/{id}/secrets/PANEL_TOKEN");
        // No key on the node: 409.
        let (status, v) = call(&app, "PUT", &uri, put(SENTINEL), None).await;
        assert_eq!(status, StatusCode::CONFLICT, "{v}");
        with_key(&mut st, KEY_A);
        let app = router(st);
        // Unapproved slot: 404.
        let (status, _) = call(
            &app,
            "PUT",
            &format!("/plugins/{id}/secrets/OTHER"),
            put(SENTINEL),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        // Too short: 400, and the answer never echoes the value.
        let (status, v) = call(&app, "PUT", &uri, put("short"), None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(!v.to_string().contains("short\""), "{v}");
        // A body that is not the expected shape: 400 without quoting it.
        let (status, v) = call(
            &app,
            "PUT",
            &uri,
            format!(r#"{{"value":"{SENTINEL}","x":1}}"#).into_bytes(),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(!v.to_string().contains(SENTINEL), "{v}");
        assert!(store.get_secret(&id, "PANEL_TOKEN").unwrap().is_none());
        // Good write.
        let (status, v) = call(&app, "PUT", &uri, put(SENTINEL), None).await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert!(!v.to_string().contains(SENTINEL));
        assert!(store.get_secret(&id, "PANEL_TOKEN").unwrap().is_some());
        // The listing carries state, never a value.
        let (status, v) = call(&app, "GET", &format!("/plugins/{id}/secrets"), vec![], None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["secrets"][0]["set"], true);
        assert_eq!(v["secrets"][0]["status"], "ok");
        assert!(!v.to_string().contains(SENTINEL));
        // Delete.
        let (status, _) = call(&app, "DELETE", &uri, vec![], None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(store.get_secret(&id, "PANEL_TOKEN").unwrap().is_none());
    }

    #[tokio::test]
    async fn deleting_an_install_removes_its_secrets() {
        let (mut st, _d) = state(None);
        with_key(&mut st, KEY_A);
        let store = st.store.clone();
        let app = router(st);
        let id = installed(&app).await;
        let uri = format!("/plugins/{id}/secrets/PANEL_TOKEN");
        let (status, _) = call(&app, "PUT", &uri, put(SENTINEL), None).await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = call(&app, "DELETE", &format!("/plugins/{id}"), vec![], None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(store.get_secret(&id, "PANEL_TOKEN").unwrap().is_none());
        assert!(store.all_secrets().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_plaintext_never_reaches_the_database_files() {
        use base64::Engine;
        let (mut st, dir) = state(None);
        with_key(&mut st, KEY_A);
        let app = router(st);
        let id = installed(&app).await;
        let (status, _) = call(
            &app,
            "PUT",
            &format!("/plugins/{id}/secrets/PANEL_TOKEN"),
            put(SENTINEL),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let b64 = base64::engine::general_purpose::STANDARD.encode(SENTINEL);
        let mut files = 0;
        for entry in walk(dir.path()) {
            let bytes = std::fs::read(&entry).unwrap_or_default();
            files += 1;
            for needle in [SENTINEL.as_bytes(), b64.as_bytes()] {
                assert!(
                    !bytes.windows(needle.len()).any(|w| w == needle),
                    "plaintext found in {}",
                    entry.display()
                );
            }
        }
        assert!(files > 0);
    }

    fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk(&p));
            } else {
                out.push(p);
            }
        }
        out
    }

    #[tokio::test]
    async fn a_key_missing_here_shows_as_held_in_the_listing() {
        let (mut st, _d) = state(None);
        with_key(&mut st, KEY_A);
        let keyring = st.keyring.clone();
        let app = router(st.clone());
        let id = installed(&app).await;
        let (status, _) = call(
            &app,
            "PUT",
            &format!("/plugins/{id}/secrets/PANEL_TOKEN"),
            put(SENTINEL),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        drop(keyring);
        // A node whose keyring lacks the key the secret was sealed under.
        let mut other = st.clone();
        with_key(&mut other, "AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=");
        let app2 = router(other);
        let (_, v) = call(
            &app2,
            "GET",
            &format!("/plugins/{id}/secrets"),
            vec![],
            None,
        )
        .await;
        let status = v["secrets"][0]["status"].as_str().unwrap();
        assert!(status.starts_with("held: key "), "{status}");
        // And one with no key at all.
        let mut bare = st;
        bare.keyring = KeyringHandle::default();
        let (_, v) = call(
            &router(bare),
            "GET",
            &format!("/plugins/{id}/secrets"),
            vec![],
            None,
        )
        .await;
        assert_eq!(v["secrets"][0]["status"], "held: no secret key");
    }
    #[tokio::test]
    async fn rotation_rewraps_under_the_new_key_and_retiring_early_holds() {
        let (mut st, _d) = state(None);
        with_key(&mut st, KEY_A);
        let app = router(st.clone());
        let id = installed(&app).await;
        let uri = format!("/plugins/{id}/secrets/PANEL_TOKEN");
        call(&app, "PUT", &uri, put(SENTINEL), None).await;
        let (_, v) = call(&app, "GET", &format!("/plugins/{id}/secrets"), vec![], None).await;
        let old_id = v["secrets"][0]["key_id"].as_str().unwrap().to_string();
        // Step 1: add the new key at the end (it becomes active), keep the old.
        let key_b = "AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=";
        with_key(&mut st, &format!("{KEY_A}\n{key_b}"));
        let app = router(st.clone());
        // Step 2: rewrap.
        let (status, r) = call(&app, "POST", "/admin/plugins/secrets/rewrap", vec![], None).await;
        assert_eq!(status, StatusCode::OK, "{r}");
        assert_eq!(r["rewrapped"], 1);
        let (_, v) = call(&app, "GET", &format!("/plugins/{id}/secrets"), vec![], None).await;
        let new_id = v["secrets"][0]["key_id"].as_str().unwrap().to_string();
        assert_ne!(old_id, new_id);
        assert_eq!(v["secrets"][0]["status"], "ok");
        // Rewrapping again is a no-op.
        let (_, r) = call(&app, "POST", "/admin/plugins/secrets/rewrap", vec![], None).await;
        assert_eq!(r["rewrapped"], 0);
        // Step 3 done too early on a node that never rewrapped: only the old key left.
        let mut early = st.clone();
        with_key(&mut early, KEY_A);
        let (_, v) = call(
            &router(early),
            "GET",
            &format!("/plugins/{id}/secrets"),
            vec![],
            None,
        )
        .await;
        assert_eq!(
            v["secrets"][0]["status"],
            format!("held: key {new_id} missing")
        );
        // The key listing shows ids only.
        let (_, k) = call(&app, "GET", "/admin/plugins/secrets/keys", vec![], None).await;
        assert!(!k.to_string().contains(key_b));
    }

    #[tokio::test]
    async fn under_ha_a_secret_is_replicated_as_ciphertext_only() {
        use base64::Engine;
        let (mut st, dir) = ha_state().await;
        with_key(&mut st, KEY_A);
        let app = router(st.clone());
        let id = installed(&app).await;
        let uri = format!("/plugins/{id}/secrets/PANEL_TOKEN");
        let (status, v) = call(&app, "PUT", &uri, put(SENTINEL), None).await;
        assert_eq!(status, StatusCode::OK, "{v}");
        let stored = st.store.get_secret(&id, "PANEL_TOKEN").unwrap().unwrap();
        assert!(!stored.sealed.ciphertext.contains(SENTINEL));
        // A snapshot built from the state machine holds ciphertext only.
        let snap = serde_json::to_string(&st.store.snapshot().unwrap()).unwrap();
        assert!(!snap.contains(SENTINEL));
        // Neither the sled files nor the Raft log hold the value or its base64.
        let b64 = base64::engine::general_purpose::STANDARD.encode(SENTINEL);
        for file in walk(dir.path()) {
            let bytes = std::fs::read(&file).unwrap_or_default();
            for needle in [SENTINEL.as_bytes(), b64.as_bytes()] {
                assert!(
                    !bytes.windows(needle.len()).any(|w| w == needle),
                    "plaintext found in {}",
                    file.display()
                );
            }
        }
        // Delete goes through the log too.
        let (status, _) = call(&app, "DELETE", &uri, vec![], None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(st.store.get_secret(&id, "PANEL_TOKEN").unwrap().is_none());
    }
    #[derive(Clone, Default)]
    struct Capture(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
        type Writer = Capture;
        fn make_writer(&'a self) -> Capture {
            self.clone()
        }
    }

    #[tokio::test]
    async fn trace_level_logs_never_carry_the_secret_but_audit_the_change() {
        let cap = Capture::default();
        let sub = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(cap.clone())
            .with_ansi(false)
            .finish();
        let _guard = tracing::subscriber::set_default(sub);
        let (mut st, _d) = state(None);
        with_key(&mut st, KEY_A);
        let app = router(st);
        let id = installed(&app).await;
        let uri = format!("/plugins/{id}/secrets/PANEL_TOKEN");
        call(&app, "PUT", &uri, put(SENTINEL), None).await;
        call(
            &app,
            "PUT",
            &uri,
            format!(r#"{{"value":"{SENTINEL}","bad":1}}"#).into_bytes(),
            None,
        )
        .await;
        call(&app, "DELETE", &uri, vec![], None).await;
        let logs = String::from_utf8(cap.0.lock().unwrap().clone()).unwrap();
        assert!(!logs.contains(SENTINEL), "{logs}");
        assert!(logs.contains("plugin secret changed"), "{logs}");
    }
    #[tokio::test]
    async fn the_route_overlay_lists_enabled_installs_routes_with_their_owner() {
        let caps = r#"{"triggers":{"on_timer":true},"tick_interval_secs":30,"log":true,"routes":{"hosts":["*.mc.example.com"],"backends":["10.0.0.0/16"],"max_entries":4}}"#;
        let (st, _d) = state(None);
        let store = st.store.clone();
        let app = router(st);
        let sha = upload(&app, &module(caps)).await;
        let (status, rec) = call(&app, "POST", "/plugins", install_body(&sha, caps), None).await;
        assert_eq!(status, StatusCode::CREATED, "{rec}");
        let id = rec["id"].as_str().unwrap().to_string();
        let set = [wayhouse_plugin_host::RouteEntry {
            host: "a.mc.example.com".into(),
            backend: "10.0.1.1:25565".into(),
        }];
        store
            .commit_state_routes(&id, 0, &std::collections::BTreeMap::new(), Some(&set))
            .unwrap();
        let (status, v) = call(&app, "GET", "/plugin-routes", vec![], None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["routes"][0]["host"], "a.mc.example.com");
        assert_eq!(v["routes"][0]["owner"], format!("plugin:{id}"));
        assert_eq!(v["routes"][0]["plugin"], "demo");
        call(
            &app,
            "POST",
            &format!("/plugins/{id}/disable"),
            vec![],
            None,
        )
        .await;
        let (_, v) = call(&app, "GET", "/plugin-routes", vec![], None).await;
        assert!(v["routes"].as_array().unwrap().is_empty());
    }
}
