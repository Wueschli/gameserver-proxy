//! Webhooks: the one endpoint that is not behind the admin bearer layer.
//!
//! `POST /plugins/<install id>/hook[/<suffix>]` is served by a **separate, optional
//! listener** (`--plugin-webhook-listen`, its own bind address and TLS, off by default), never
//! on the admin port. Each install gets its own bearer token, shown once and stored as a
//! SHA-256 hash in the replicated install record. In order, a request is:
//!
//! 1. counted against a per-source rate limit (before anything is compared, so a token cannot
//!    be guessed at line rate);
//! 2. authenticated by constant-time comparison of the token's hash (an unknown install, a
//!    disabled one and a wrong token all answer `401` alike), **before the body is read**;
//! 3. counted against a per-install rate limit, then its body is read up to a cap;
//! 4. run on the Raft leader. A follower authenticated the caller itself and forwards the
//!    request over the authenticated peer channel, only to an `https://` peer (the body and
//!    headers would otherwise cross the network in clear text): otherwise `503`.
//!
//! The answer is the guest's, but only after its effects were committed: a commit that
//! failed is `503` with `Retry-After`, a full queue `429`. Effects of a retried request can
//! repeat, so every call carries an idempotency key (the caller's `Idempotency-Key`, else a
//! hash of the install and body) for the guest to forward.

use std::collections::{BTreeMap, HashMap};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::body::Body;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use wayhouse_plugin_host::webhook::MAX_WEBHOOK_BODY;
use wayhouse_plugin_host::{WebhookRequest, WebhookResponse};

use super::runner::{Hook, HookError, Runner};
use super::{valid_id, PluginStore};
use crate::ha::peers::{is_plain_remote, peer_url};
use crate::ha::HaHandle;

/// Requests per second and burst one source address may make, valid or not.
const SOURCE_RATE: (f64, f64) = (5.0, 20.0);
/// Requests per second and burst one install's webhook accepts.
const INSTALL_RATE: (f64, f64) = (10.0, 20.0);
/// Headers the guest is shown at most.
const MAX_HEADERS: usize = 32;

/// A new webhook token: 256 random bits, URL-safe.
pub fn new_token() -> String {
    let mut raw = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut raw);
    format!("whk_{}", URL_SAFE_NO_PAD.encode(raw))
}

/// The hash stored in the install record.
pub fn hash_token(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A token bucket per key.
pub struct Limiter<K> {
    rate: f64,
    burst: f64,
    buckets: Mutex<HashMap<K, (f64, Instant)>>,
}

impl<K: std::hash::Hash + Eq + Clone> Limiter<K> {
    pub fn new((rate, burst): (f64, f64)) -> Self {
        Self {
            rate,
            burst,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Takes one token for `key` at `now`; `false` when its bucket is empty.
    pub fn allow(&self, key: &K, now: Instant) -> bool {
        let mut all = self
            .buckets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if all.len() > 50_000 {
            // Buckets that have refilled fully carry no information.
            let (rate, burst) = (self.rate, self.burst);
            all.retain(|_, (t, at)| {
                *t + rate * now.saturating_duration_since(*at).as_secs_f64() < burst
            });
        }
        let entry = all.entry(key.clone()).or_insert((self.burst, now));
        let refilled = entry.0 + self.rate * now.saturating_duration_since(entry.1).as_secs_f64();
        entry.0 = refilled.min(self.burst);
        entry.1 = now;
        if entry.0 >= 1.0 {
            entry.0 -= 1.0;
            true
        } else {
            false
        }
    }
}

/// An IPv6 source counts by its /64.
fn source_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => {
            let mut o = v6.octets();
            o[8..].fill(0);
            IpAddr::V6(o.into())
        }
        v4 => v4,
    }
}

#[derive(Clone)]
pub struct HookState {
    pub store: PluginStore,
    pub runner: Arc<Runner>,
    pub ha: Option<Arc<HaHandle>>,
    source: Arc<Limiter<IpAddr>>,
    install: Arc<Limiter<String>>,
}

impl HookState {
    pub fn new(store: PluginStore, runner: Arc<Runner>, ha: Option<Arc<HaHandle>>) -> Self {
        Self {
            store,
            runner,
            ha,
            source: Arc::new(Limiter::new(SOURCE_RATE)),
            install: Arc::new(Limiter::new(INSTALL_RATE)),
        }
    }
}

/// The webhook listener's app: only the hook route.
pub fn router(state: HookState) -> Router {
    Router::new()
        .route("/plugins/{id}/hook", post(hook))
        .route("/plugins/{id}/hook/{*suffix}", post(hook_suffix))
        // The body is read by hand after the token is checked.
        .layer(DefaultBodyLimit::disable())
        .with_state(state)
}

fn json_error(status: StatusCode, msg: &str) -> Response {
    let mut resp = (status, Json(serde_json::json!({ "error": msg }))).into_response();
    if matches!(
        status,
        StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE
    ) {
        resp.headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    }
    resp
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

/// Headers the guest does not see: credentials.
fn hidden(name: &str) -> bool {
    matches!(
        name,
        "authorization" | "cookie" | "proxy-authorization" | "set-cookie"
    )
}

fn guest_headers(headers: &HeaderMap) -> BTreeMap<String, String> {
    headers
        .iter()
        .filter(|(n, _)| !hidden(n.as_str()))
        .filter_map(|(n, v)| {
            let v = v.to_str().ok()?;
            (v.len() <= 1024).then(|| (n.as_str().to_string(), v.to_string()))
        })
        .take(MAX_HEADERS)
        .collect()
}

/// The caller's `Idempotency-Key`, else a hash of the install and the body.
fn idempotency_key(headers: &HeaderMap, id: &str, body: &[u8]) -> String {
    if let Some(k) = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .filter(|k| (1..=128).contains(&k.len()) && k.bytes().all(|b| b.is_ascii_graphic()))
    {
        return k.to_string();
    }
    let mut h = Sha256::new();
    h.update(id.as_bytes());
    h.update([0]);
    h.update(body);
    h.finalize()
        .iter()
        .take(16)
        .map(|b| format!("{b:02x}"))
        .collect()
}

async fn hook(
    state: State<HookState>,
    peer: ConnectInfo<wayhouse_http::tls::PeerAddr>,
    Path(id): Path<String>,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> Response {
    serve(
        state.0,
        peer.0 .0.ip(),
        id,
        String::new(),
        uri,
        headers,
        body,
    )
    .await
}

async fn hook_suffix(
    state: State<HookState>,
    peer: ConnectInfo<wayhouse_http::tls::PeerAddr>,
    Path((id, suffix)): Path<(String, String)>,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> Response {
    serve(
        state.0,
        peer.0 .0.ip(),
        id,
        format!("/{suffix}"),
        uri,
        headers,
        body,
    )
    .await
}

async fn serve(
    st: HookState,
    source: IpAddr,
    id: String,
    suffix: String,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let now = Instant::now();
    // 1. Before any comparison.
    if !st.source.allow(&source_key(source), now) {
        return json_error(
            StatusCode::TOO_MANY_REQUESTS,
            "too many requests from this address",
        );
    }
    // 2. Authenticate, before the body. Every failure looks the same.
    let unauthorized = || json_error(StatusCode::UNAUTHORIZED, "unauthorized");
    let Some(token) = bearer(&headers) else {
        return unauthorized();
    };
    let presented = hash_token(token);
    let stored = if valid_id(&id) {
        st.store.get(&id).ok().flatten()
    } else {
        None
    };
    // Always compare, even with nothing to compare against.
    let expected = stored
        .as_ref()
        .and_then(|r| r.webhook.as_ref())
        .map_or("0".repeat(64), |w| w.token_hash.clone());
    let matches = wayhouse_http::token_eq(&presented, &expected);
    let live = stored
        .as_ref()
        .is_some_and(|r| r.enabled && r.webhook.is_some());
    if !(matches && live) {
        return unauthorized();
    }
    // 3. Per-install limit, then the body.
    if !st.install.allow(&id, now) {
        return json_error(
            StatusCode::TOO_MANY_REQUESTS,
            "this webhook is being called too often",
        );
    }
    let Ok(body) = axum::body::to_bytes(body, MAX_WEBHOOK_BODY).await else {
        return json_error(StatusCode::PAYLOAD_TOO_LARGE, "the body is over 256 KiB");
    };
    let request = WebhookRequest {
        method: "POST".into(),
        suffix,
        query: uri.query().unwrap_or_default().to_string(),
        headers: guest_headers(&headers),
        idempotency_key: idempotency_key(&headers, &id, &body),
        body: body.to_vec(),
    };
    dispatch(&st, &id, request).await
}

/// Runs the call here if this node leads, else on the leader.
async fn dispatch(st: &HookState, id: &str, request: WebhookRequest) -> Response {
    match st.runner.run_hook(id, Hook::Webhook(request.clone())).await {
        Ok(reply) => reply_response(reply.unwrap_or_default()),
        Err(HookError::NotLeader) => match &st.ha {
            Some(ha) => forward(ha, id, &request).await,
            None => json_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "this node is not running plugins",
            ),
        },
        Err(e) => error_response(&e),
    }
}

fn reply_response(reply: WebhookResponse) -> Response {
    let mut resp = Response::new(Body::from(reply.body));
    *resp.status_mut() = StatusCode::from_u16(reply.status).unwrap_or(StatusCode::OK);
    for (k, v) in reply.headers {
        if let (Ok(name), Ok(value)) = (HeaderName::try_from(k), HeaderValue::from_str(&v)) {
            resp.headers_mut().insert(name, value);
        }
    }
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

fn error_response(e: &HookError) -> Response {
    match e {
        HookError::NotFound => json_error(StatusCode::NOT_FOUND, "no such webhook"),
        HookError::NotLeader => {
            json_error(StatusCode::SERVICE_UNAVAILABLE, "not the leader; retry")
        }
        HookError::QueueFull => {
            json_error(StatusCode::TOO_MANY_REQUESTS, "the plugin is busy; retry")
        }
        HookError::Unavailable(why) => json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            &format!("not committed, retry: {why}"),
        ),
        HookError::Failed => json_error(StatusCode::BAD_GATEWAY, "the plugin failed"),
    }
}

// ---- follower to leader ----

#[derive(Serialize, Deserialize)]
struct ForwardedHook {
    request: WebhookRequest,
}

/// The leader's answer to a forwarded hook.
#[derive(Serialize, Deserialize)]
struct ForwardedReply {
    status: u16,
    headers: BTreeMap<String, String>,
    body: String,
}

/// `POST /raft/plugin-hook/{id}` on the leader, behind the peer token and protocol gate.
pub fn peer_router(ha: &Arc<HaHandle>, runner: Arc<Runner>) -> Router {
    let routes = Router::new()
        .route(
            "/raft/plugin-hook/{id}",
            post(run_forwarded).layer(DefaultBodyLimit::max(MAX_WEBHOOK_BODY * 2)),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            wayhouse_http::server::BearerAuth::new(ha.ha_token.as_deref()),
            wayhouse_http::server::require_bearer,
        ));
    wayhouse_http::protocol::gate(routes, "raft").with_state(runner)
}

async fn run_forwarded(
    State(runner): State<Arc<Runner>>,
    Path(id): Path<String>,
    Json(fwd): Json<ForwardedHook>,
) -> Response {
    if !valid_id(&id) || fwd.request.body.len() > MAX_WEBHOOK_BODY {
        return json_error(StatusCode::BAD_REQUEST, "bad forwarded hook");
    }
    match runner.run_hook(&id, Hook::Webhook(fwd.request)).await {
        Ok(reply) => {
            let r = reply.unwrap_or_default();
            Json(ForwardedReply {
                status: r.status,
                headers: r.headers,
                body: STANDARD.encode(r.body),
            })
            .into_response()
        }
        Err(e) => error_response(&e),
    }
}

async fn forward(ha: &HaHandle, id: &str, request: &WebhookRequest) -> Response {
    let leader = {
        let metrics = ha.raft.metrics().borrow().clone();
        metrics.current_leader.and_then(|l| {
            (l != ha.node_id)
                .then(|| metrics.membership_config.membership().get_node(&l).cloned())
                .flatten()
        })
    };
    let Some(leader) = leader else {
        return json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "no leader to run the webhook; retry",
        );
    };
    if is_plain_remote(&leader.addr) {
        // The body and headers would cross the network in clear text.
        return json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "webhooks reach the leader over https peers only; this cluster's peers are plain http",
        );
    }
    let mut req = ha
        .forward
        .post(peer_url(&leader.addr, &format!("/raft/plugin-hook/{id}")))
        .json(&ForwardedHook {
            request: request.clone(),
        });
    if let Some(token) = &ha.ha_token {
        req = req.bearer_auth(token);
    }
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %wayhouse_http::error_chain(&e), "could not forward a webhook to the leader");
            return json_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "could not reach the leader; retry",
            );
        }
    };
    let status = resp.status();
    if status.is_success() {
        return match resp.json::<ForwardedReply>().await.ok().and_then(|r| {
            STANDARD.decode(&r.body).ok().map(|body| WebhookResponse {
                status: r.status,
                headers: r.headers,
                body,
            })
        }) {
            Some(r) => reply_response(r),
            None => json_error(
                StatusCode::BAD_GATEWAY,
                "the leader's answer was not understood",
            ),
        };
    }
    // Pass the leader's verdict on, with its own message (never a plugin's internals).
    let msg = resp
        .json::<serde_json::Value>()
        .await
        .ok()
        .and_then(|v| v["error"].as_str().map(str::to_string))
        .unwrap_or_else(|| "the leader refused the webhook".into());
    match status {
        StatusCode::TOO_MANY_REQUESTS | StatusCode::NOT_FOUND | StatusCode::BAD_GATEWAY => {
            json_error(status, &msg)
        }
        _ => json_error(StatusCode::SERVICE_UNAVAILABLE, &msg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn tokens_are_long_unique_and_hash_stably() {
        let (a, b) = (new_token(), new_token());
        assert_ne!(a, b);
        assert!(a.starts_with("whk_") && a.len() > 40);
        assert_eq!(hash_token(&a), hash_token(&a));
        assert_eq!(hash_token(&a).len(), 64);
        assert_ne!(hash_token(&a), hash_token(&b));
    }

    #[test]
    fn a_bucket_allows_its_burst_then_refills() {
        let l = Limiter::new((2.0, 3.0));
        let t0 = Instant::now();
        let k = "x".to_string();
        assert!([(); 3].iter().all(|()| l.allow(&k, t0)));
        assert!(!l.allow(&k, t0));
        assert!(!l.allow(&k, t0 + Duration::from_millis(100)));
        assert!(l.allow(&k, t0 + Duration::from_millis(600)));
        // Another key has its own bucket.
        assert!(l.allow(&"y".to_string(), t0));
    }

    #[test]
    fn ipv6_sources_share_a_bucket_per_64() {
        let a: IpAddr = "2001:db8::1".parse().unwrap();
        let b: IpAddr = "2001:db8::ffff".parse().unwrap();
        let c: IpAddr = "2001:db8:0:1::1".parse().unwrap();
        assert_eq!(source_key(a), source_key(b));
        assert_ne!(source_key(a), source_key(c));
    }

    #[test]
    fn credentials_never_reach_the_guest_and_the_key_is_stable() {
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, "Bearer secret".parse().unwrap());
        h.insert(header::COOKIE, "a=b".parse().unwrap());
        h.insert("x-event", "server.created".parse().unwrap());
        let g = guest_headers(&h);
        assert_eq!(g.len(), 1);
        assert_eq!(g["x-event"], "server.created");
        let k1 = idempotency_key(&h, "id", b"body");
        assert_eq!(k1, idempotency_key(&h, "id", b"body"));
        assert_ne!(k1, idempotency_key(&h, "id", b"other"));
        h.insert("idempotency-key", "abc-123".parse().unwrap());
        assert_eq!(idempotency_key(&h, "id", b"body"), "abc-123");
    }
}
