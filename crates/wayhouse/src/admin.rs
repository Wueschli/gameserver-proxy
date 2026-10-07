//! The admin / observability HTTP API. Bound to an internal address only.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::StatusCode,
    middleware,
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post},
    Json, Router,
};
use metrics_exporter_prometheus::PrometheusHandle;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use wayhouse_core::pool::AdminState as BackendState;
use wayhouse_core::sniff::Sniffers;
use wayhouse_core::RuntimeHandle;

#[derive(Clone)]
struct AdminState {
    runtime: RuntimeHandle,
    prometheus: PrometheusHandle,
    /// Bearer token every request except `GET /healthz` must present;
    /// `None` leaves the API open (`settings.admin.auth_token`, phase 10+11
    /// slice 10 — see `docs/05` "Admin API auth").
    auth_token: Option<String>,
    /// `settings.sniffers.dir`, if configured — where `/admin/sniffers`
    /// writes/removes `.wasm` files. `None` when this instance has no
    /// `settings.sniffers` block at all (turning sniffing on from nothing is
    /// still startup-only; see `sniffers_disabled`).
    sniffers_dir: Option<PathBuf>,
    sniffers: std::sync::Arc<Sniffers>,
    /// The live `settings.sniffers.modules` pins (follows reloads), so an
    /// upload the next rescan would reject never reaches the disk.
    sniffer_pins: SnifferPins,
    /// Vets an uploaded module before it is written. `None` when this build or
    /// instance cannot load sniffers at all.
    sniffer_validator: Option<SnifferValidator>,
}

pub type SnifferPins =
    std::sync::Arc<dyn Fn() -> Vec<wayhouse_config::SnifferModulePin> + Send + Sync>;
pub type SnifferValidator = std::sync::Arc<dyn Fn(&[u8]) -> Result<(), String> + Send + Sync>;

/// The admin API route table. Split out from [`serve`] so integration tests can
/// mount it on their own ephemeral listener. `/healthz` alone stays outside
/// the [`wayhouse_http::server::require_bearer`] gate — plain liveness-probe convention, same
/// choice `wayhouse-controller`/`wayhouse-aggregator` made for their own `/healthz`.
fn router(state: AdminState) -> Router {
    let gated = Router::new()
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics))
        .route("/pools", get(pools))
        .route("/pools/{pool}/backends", post(add_backend))
        .route(
            "/pools/{pool}/backends/{addr}",
            patch(patch_backend).delete(delete_backend),
        )
        .route("/config", get(config))
        .route("/sessions", get(sessions))
        .route("/admin/drain", post(drain))
        .route("/admin/undrain", post(undrain))
        .route("/route-hint", post(route_hint))
        .route(
            "/admin/sniffers",
            get(list_sniffers)
                .post(upload_sniffer)
                .layer(DefaultBodyLimit::max(
                    crate::sniffer_loader::MAX_MODULE_BYTES,
                )),
        )
        .route("/admin/sniffers/{name}", delete(delete_sniffer))
        .route("/admin/sniffers/{name}/rollback", post(rollback_sniffer))
        .route_layer(middleware::from_fn_with_state(
            wayhouse_http::server::BearerAuth::new(state.auth_token.as_deref())
                .with_body("unauthorized\n"),
            wayhouse_http::server::require_bearer,
        ));

    // The aggregator's fan-out (and the controller's tooling) call these with the
    // protocol header; operators also call them by hand, so a missing header is
    // fine but another major is refused.
    let gated = wayhouse_http::protocol::gate_lenient(gated, "proxy");
    Router::new()
        .route("/healthz", get(healthz))
        .merge(gated)
        .with_state(state)
}

/// The admin API's TLS handshake limits: the defaults, with each
/// `settings.admin.tls` limit that is set on top.
pub fn handshake_limits(
    tls: Option<&wayhouse_config::AdminTls>,
) -> wayhouse_http::tls::HandshakeLimits {
    let d = wayhouse_http::tls::HandshakeLimits::default();
    let Some(t) = tls else { return d };
    wayhouse_http::tls::HandshakeLimits {
        max_pending: t.max_pending.unwrap_or(d.max_pending),
        max_pending_per_source: t.max_pending_per_source.unwrap_or(d.max_pending_per_source),
        new_per_source_per_sec: t.new_per_source_per_sec.unwrap_or(d.new_per_source_per_sec),
        new_per_source_burst: t.new_per_source_burst.unwrap_or(d.new_per_source_burst),
        ..d
    }
}

#[allow(clippy::too_many_arguments)] // flat wiring of independent `main.rs` inputs
pub async fn serve(
    addr: SocketAddr,
    tls: Option<(
        std::sync::Arc<wayhouse_http::tls::ReloadingCert>,
        wayhouse_http::tls::HandshakeLimits,
    )>,
    runtime: RuntimeHandle,
    prometheus: PrometheusHandle,
    auth_token: Option<String>,
    sniffers_dir: Option<PathBuf>,
    sniffers: std::sync::Arc<Sniffers>,
    sniffer_pins: SnifferPins,
    sniffer_validator: Option<SnifferValidator>,
) {
    let app = router(AdminState {
        runtime,
        prometheus,
        auth_token,
        sniffers_dir,
        sniffers,
        sniffer_pins,
        sniffer_validator,
    });

    // HTTPS with `settings.admin.tls`, plain HTTP otherwise. A bind failure
    // lands here too and, as before, ends only this task (non-fatal).
    let (cert, limits) = match tls {
        Some((cert, limits)) => (Some(cert), limits),
        None => (None, wayhouse_http::tls::HandshakeLimits::default()),
    };
    if let Err(e) = wayhouse_http::tls::serve(addr, app, cert, limits, "admin API").await {
        tracing::error!(%addr, error = %e, "admin API server error");
    }
}

async fn healthz() -> &'static str {
    "ok"
}

async fn readyz(State(s): State<AdminState>) -> impl IntoResponse {
    if s.runtime.ready() {
        (StatusCode::OK, "ready")
    } else if s.runtime.is_draining() {
        (StatusCode::SERVICE_UNAVAILABLE, "draining")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not ready")
    }
}

/// `POST /admin/drain` — take this instance out of LB rotation (`readyz` starts
/// failing) without stopping the data path. In-flight sessions keep running;
/// operators wait for `active_conns` to fall, then send `SIGTERM`.
async fn drain(State(s): State<AdminState>) -> (StatusCode, String) {
    s.runtime.set_draining(true);
    let n = s.runtime.active_conns();
    tracing::info!(active_conns = n, "instance marked draining via admin API");
    (StatusCode::OK, format!("draining; active_conns={n}\n"))
}

/// `POST /admin/undrain` — put the instance back into rotation.
async fn undrain(State(s): State<AdminState>) -> (StatusCode, &'static str) {
    s.runtime.set_draining(false);
    tracing::info!("instance returned to rotation via admin API");
    (StatusCode::OK, "ready\n")
}

/// `GET /config` — a plaintext view of the active snapshot (listeners + pools).
async fn config(State(s): State<AdminState>) -> impl IntoResponse {
    fn opt(v: Option<impl std::fmt::Display>) -> String {
        v.map_or_else(|| "-".to_string(), |n| n.to_string())
    }
    let snap = s.runtime.snapshot();
    let mut out = String::new();
    let lim = &snap.limits;
    out.push_str(&format!(
        "draining={}\tactive_conns={}\tlimits=conn:{},udp:{},new_rate:{}\tgeo_db={}\n\nlisteners:\n",
        s.runtime.is_draining(),
        s.runtime.active_conns(),
        opt(lim.max_connections),
        opt(lim.max_udp_sessions),
        opt(lim.max_new_sessions_per_sec),
        snap.geo_db.as_deref().unwrap_or("-"),
    ));
    for l in &snap.listeners {
        let bind_display = if l.extra_binds.is_empty() {
            l.bind.to_string()
        } else {
            // Port-range bind (F1.4): one real socket per port, shown compactly
            // rather than listing every address.
            format!("{} (+{} ports)", l.bind, l.extra_binds.len())
        };
        out.push_str(&format!(
            "  {}\tbind={}\tproto={:?}\troutes={}{}{}{}{}{}{}{}{}{}{}\n",
            l.name,
            bind_display,
            l.protocol,
            l.routes.len(),
            if l.route_hint { "\troute_hint" } else { "" },
            if l.freebind { "\tfreebind" } else { "" },
            if l.transparent { "\ttransparent" } else { "" },
            if l.first_packet_gate {
                "\tfirst_packet_gate"
            } else {
                ""
            },
            if l.acl.is_empty() {
                String::new()
            } else {
                format!("\tacl=+{}/-{}", l.acl.allow.len(), l.acl.deny.len())
            },
            match &l.geo {
                Some(g) => format!("\tgeo=+{}/-{}", g.allow.len(), g.deny.len()),
                None => String::new(),
            },
            match &l.rate_limit {
                Some(rl) => {
                    let f = |b: Option<wayhouse_config::TokenBucket>| match b {
                        Some(b) => format!("{}/{}", b.rate, b.burst),
                        None => "-".to_string(),
                    };
                    format!("\trate_limit=ip:{},net:{}", f(rl.per_ip), f(rl.per_net))
                }
                None => String::new(),
            },
            match &l.per_source {
                Some(ps) => {
                    let g = |v: Option<usize>| v.map_or("-".to_string(), |n| n.to_string());
                    format!(
                        "\tper_source=ip:{},net:{}",
                        g(ps.max_per_ip),
                        g(ps.max_per_net)
                    )
                }
                None => String::new(),
            },
            match &l.prefix {
                Some(p) => format!("\tprefix={p:?}"),
                None => String::new(),
            },
            if l.sniffers.is_empty() {
                String::new()
            } else {
                format!("\tsniffers={}", l.sniffers.join(","))
            },
        ));
    }
    out.push_str("\npools:\n");
    for (name, pool) in &snap.pools {
        let (added, removed) = s.runtime.backend_overlay().pending(name);
        let ov = if added.is_empty() && removed.is_empty() {
            String::new()
        } else {
            format!("\toverlay=+{}/-{}", added.len(), removed.len())
        };
        out.push_str(&format!("  {name}\tbalancer={:?}{ov}\n", pool.balancer));
        for b in pool.backends() {
            out.push_str(&format!(
                "    {}\t{}\tstate={}\tactive={}\n",
                b.addr,
                if b.is_healthy() {
                    "healthy"
                } else {
                    "unhealthy"
                },
                b.admin_state().as_str(),
                b.active(),
            ));
        }
    }
    out
}

/// `GET /sessions[?listener=&pool=&proto=tcp|udp&src=<ip>]` — a plaintext list
/// of every live proxied connection / UDP session (id, transport, listener,
/// client / local address, chosen pool and backend, age). The query params
/// filter the list (exact match on `listener` / `pool`, transport on `proto`,
/// source IP on `src`). Point-in-time; a session already gone by the time you
/// read this is simply absent.
#[derive(Deserialize)]
struct SessionsQuery {
    listener: Option<String>,
    pool: Option<String>,
    proto: Option<String>,
    src: Option<String>,
}

async fn sessions(
    State(s): State<AdminState>,
    Query(q): Query<SessionsQuery>,
) -> impl IntoResponse {
    let src = q.src.as_deref().and_then(|s| s.parse::<IpAddr>().ok());
    let mut list = s.runtime.sessions();
    list.retain(|e| {
        q.listener.as_deref().is_none_or(|l| l == e.listener)
            && q.pool
                .as_deref()
                .is_none_or(|p| Some(p) == e.pool.as_deref())
            && q.proto.as_deref().is_none_or(|p| p == e.proto.as_str())
            && src.is_none_or(|ip| ip == e.peer.ip())
    });
    list.sort_by_key(|e| e.id);
    let mut out = format!("sessions={}\n", list.len());
    for e in &list {
        out.push_str(&format!(
            "  {}\t{}\t{}\tpeer={}\tlocal={}\tpool={}\tbackend={}\tage={:.1}s\n",
            e.id,
            e.proto.as_str(),
            e.listener,
            e.peer,
            e.local,
            e.pool.as_deref().unwrap_or("-"),
            e.backend.map_or_else(|| "-".to_string(), |a| a.to_string()),
            e.age.as_secs_f64(),
        ));
    }
    out
}

async fn metrics(State(s): State<AdminState>) -> impl IntoResponse {
    s.prometheus.render()
}

/// `POST /route-hint` — the launcher / control-plane push resolver (scheme C).
/// Records a short-lived `src_ip → pool` hint that listeners with
/// `route_hint: true` consult before their route list.
#[derive(Deserialize)]
struct RouteHintReq {
    src_ip: String,
    pool: String,
    #[serde(default = "default_ttl_sec")]
    ttl_sec: u64,
}

fn default_ttl_sec() -> u64 {
    30
}

async fn route_hint(
    State(s): State<AdminState>,
    Json(req): Json<RouteHintReq>,
) -> (StatusCode, &'static str) {
    let Ok(ip) = req.src_ip.parse::<IpAddr>() else {
        return (
            StatusCode::BAD_REQUEST,
            "src_ip is not a valid IP address\n",
        );
    };
    if req.ttl_sec == 0 || req.ttl_sec > 3600 {
        return (
            StatusCode::BAD_REQUEST,
            "ttl_sec must be between 1 and 3600\n",
        );
    }
    if s.runtime.snapshot().pool(&req.pool).is_none() {
        return (StatusCode::BAD_REQUEST, "unknown pool\n");
    }
    s.runtime
        .route_hints()
        .set(ip, req.pool, Duration::from_secs(req.ttl_sec));
    (StatusCode::OK, "ok\n")
}

async fn pools(State(s): State<AdminState>) -> impl IntoResponse {
    let snap = s.runtime.snapshot();
    let mut out = String::new();
    for (name, pool) in &snap.pools {
        out.push_str(&format!("{name}\tbalancer={:?}\n", pool.balancer));
        for b in pool.backends() {
            out.push_str(&format!(
                "  {}\t{}\tstate={}\tactive={}\n",
                b.addr,
                if b.is_healthy() {
                    "healthy"
                } else {
                    "unhealthy"
                },
                b.admin_state().as_str(),
                b.active(),
            ));
        }
    }
    out
}

/// `PATCH /pools/{pool}/backends/{addr}` `{ "state": "enabled"|"draining"|"disabled" }`
/// — set an operator backend state. `draining` / `disabled` stop new-session
/// selection while existing sessions keep running.
#[derive(Deserialize)]
struct BackendPatch {
    state: String,
}

async fn patch_backend(
    State(s): State<AdminState>,
    Path((pool, addr)): Path<(String, String)>,
    Json(req): Json<BackendPatch>,
) -> (StatusCode, String) {
    let Some(state) = BackendState::parse(&req.state) else {
        return (
            StatusCode::BAD_REQUEST,
            "state must be one of: enabled, draining, disabled\n".into(),
        );
    };
    let Ok(addr) = addr.parse::<SocketAddr>() else {
        return (
            StatusCode::BAD_REQUEST,
            "backend address is not a valid ip:port\n".into(),
        );
    };
    let snap = s.runtime.snapshot();
    let Some(pool) = snap.pool(&pool) else {
        return (StatusCode::NOT_FOUND, "unknown pool\n".into());
    };
    let Some(backend) = pool.backend(addr) else {
        return (StatusCode::NOT_FOUND, "unknown backend in pool\n".into());
    };
    backend.set_admin_state(state);
    tracing::info!(pool = %pool.name, %addr, state = state.as_str(), "backend admin state changed");
    (StatusCode::OK, format!("{addr} -> {}\n", state.as_str()))
}

/// `POST /pools/{pool}/backends` `{ "addr": "10.0.0.5:7777" }` — add a backend
/// to a pool at runtime. The edit lives in the runtime overlay (it survives a
/// file reload); the snapshot is rebuilt so the backend joins immediately and
/// the health checker corrects its state within one interval.
#[derive(Deserialize)]
struct BackendAdd {
    addr: String,
}

async fn add_backend(
    State(s): State<AdminState>,
    Path(pool): Path<String>,
    Json(req): Json<BackendAdd>,
) -> (StatusCode, String) {
    let Ok(addr) = req.addr.parse::<SocketAddr>() else {
        return (
            StatusCode::BAD_REQUEST,
            "addr is not a valid ip:port\n".into(),
        );
    };
    if s.runtime.snapshot().pool(&pool).is_none() {
        return (StatusCode::NOT_FOUND, "unknown pool\n".into());
    }
    s.runtime.backend_overlay().add(&pool, addr);
    s.runtime.request_reload();
    tracing::info!(%pool, %addr, "backend added via admin API");
    (StatusCode::OK, format!("added {addr} to {pool}\n"))
}

/// `DELETE /pools/{pool}/backends/{addr}` — remove a backend from a pool at
/// runtime (works whether it came from the file or a prior `POST`). Existing
/// sessions on it keep running; it just stops being selected.
async fn delete_backend(
    State(s): State<AdminState>,
    Path((pool, addr)): Path<(String, String)>,
) -> (StatusCode, String) {
    let Ok(addr) = addr.parse::<SocketAddr>() else {
        return (
            StatusCode::BAD_REQUEST,
            "backend address is not a valid ip:port\n".into(),
        );
    };
    if s.runtime.snapshot().pool(&pool).is_none() {
        return (StatusCode::NOT_FOUND, "unknown pool\n".into());
    }
    s.runtime.backend_overlay().remove(&pool, addr);
    s.runtime.request_reload();
    tracing::info!(%pool, %addr, "backend removed via admin API");
    (StatusCode::OK, format!("removed {addr} from {pool}\n"))
}

/// A module name safe to join onto `sniffers_dir` — no path separators, no
/// `..`, non-empty. Rejects anything else rather than trying to sanitize it,
/// since this name becomes a filename on disk from an admin-API caller.
fn valid_module_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[derive(Serialize, Deserialize)]
struct SnifferInfo {
    name: String,
    sha256: String,
    size_bytes: u64,
    /// Whether this module is actually loaded into the live registry right
    /// now (a file can exist on disk but have failed its last scan — e.g. a
    /// hash-pin mismatch — in which case this is `false`).
    loaded: bool,
    /// A previous version is kept (`POST /admin/sniffers/{name}/rollback`).
    #[serde(default)]
    has_previous: bool,
    /// The live instance runs the previous version because the current file
    /// failed validation at the last scan.
    #[serde(default)]
    fallback: bool,
}

/// `GET /admin/sniffers` — lists every `.wasm` file in `settings.sniffers.dir`
/// (name, sha256, size), plus whether it's currently loaded into the live
/// registry. `409` if this instance has no `settings.sniffers` configured at
/// all.
async fn list_sniffers(State(s): State<AdminState>) -> Response {
    let Some(dir) = &s.sniffers_dir else {
        return sniffers_disabled();
    };
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("reading sniffers.dir: {e}\n"),
            )
                .into_response()
        }
    };
    let loaded = s.sniffers.names();
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("wasm") {
            continue;
        }
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        out.push(SnifferInfo {
            sha256: format!("{:x}", Sha256::digest(&bytes)),
            size_bytes: bytes.len() as u64,
            loaded: loaded.contains(&name),
            has_previous: prev_path(dir, &name).is_file(),
            fallback: s.sniffers.get(&name).is_some_and(|x| x.is_fallback()),
            name,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Json(out).into_response()
}

/// `POST /admin/sniffers?name=<module>` — validates a `.wasm` module's raw
/// bytes (a loadable sniffer module within `MAX_MODULE_BYTES`; else `400`, or
/// `413` past the body limit), checks them against `settings.sniffers.modules`
/// pins when the instance has any (else `409`, because the next rescan would
/// reject the file and a stray unpinned file makes the next startup fatal),
/// then writes `settings.sniffers.dir/<name>.wasm` atomically (temp file +
/// rename) and requests a reload so `SnifferLoader::scan` picks it up — the
/// same hot mechanism `POST /pools/{pool}/backends` uses for the runtime
/// overlay. Nothing is written when any check fails. Updating the pin list
/// itself is a config change, out of scope for this endpoint.
#[derive(Deserialize)]
struct SnifferUploadQuery {
    name: String,
}

/// The kept previous version of `<name>.wasm`: a dotfile, so the `*.wasm` scan
/// and the listing never treat it as a module of its own.
pub(crate) fn prev_path(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    dir.join(format!(".{name}.wasm.prev"))
}

/// Serialises every swap of `<name>.wasm` / `.prev` (upload, rollback, delete).
static SWAP_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn sha_of_file(path: &std::path::Path) -> Option<String> {
    std::fs::read(path)
        .ok()
        .map(|b| format!("{:x}", Sha256::digest(&b)))
}

/// Uniquifies upload temp files so concurrent uploads never share one.
static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

async fn upload_sniffer(
    State(s): State<AdminState>,
    Query(q): Query<SnifferUploadQuery>,
    body: Bytes,
) -> Response {
    let Some(dir) = &s.sniffers_dir else {
        return sniffers_disabled();
    };
    if !valid_module_name(&q.name) {
        return (
            StatusCode::BAD_REQUEST,
            "name must be non-empty and contain only [A-Za-z0-9_-]\n".to_string(),
        )
            .into_response();
    }
    let Some(validator) = s.sniffer_validator.clone() else {
        return sniffers_disabled();
    };
    let bytes = body.clone();
    // Compiling a module is CPU work; keep it off the async workers.
    let v0 = validator.clone();
    let verdict = tokio::task::spawn_blocking(move || v0(&bytes)).await;
    match verdict {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("invalid sniffer module: {e}\n"),
            )
                .into_response()
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("validating the module: {e}\n"),
            )
                .into_response()
        }
    }
    let pins = (s.sniffer_pins)();
    if !pins.is_empty() {
        let digest = format!("{:x}", Sha256::digest(&body));
        let refusal = match pins.iter().find(|p| p.name == q.name) {
            Some(p) if p.sha256 == digest => None,
            Some(p) => Some(format!(
                "pinned: {} must have sha256 {}, the upload has {digest}\n",
                q.name, p.sha256
            )),
            None => Some(format!(
                "pinned: {} is not listed in settings.sniffers.modules (upload sha256 {digest})\n",
                q.name
            )),
        };
        if let Some(msg) = refusal {
            return (StatusCode::CONFLICT, msg).into_response();
        }
    }
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        q.name,
        TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let path = dir.join(format!("{}.wasm", q.name));
    let new_sha = format!("{:x}", Sha256::digest(&body));
    let prev = prev_path(dir, &q.name);
    // The whole swap, including the decision to keep the current file, runs
    // under SWAP_LOCK on a blocking thread (it validates a module), so
    // concurrent uploads each see the file the other installed.
    let written = {
        let (tmp, path, prev, dir, name, body, validator) = (
            tmp.clone(),
            path.clone(),
            prev,
            dir.clone(),
            q.name.clone(),
            body.clone(),
            validator.clone(),
        );
        tokio::task::spawn_blocking(move || {
            let _guard = SWAP_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::fs::write(&tmp, &body).and_then(|()| {
                // Keep the module being replaced when it is itself a loadable
                // module (a rejected one must not displace the known-good
                // previous) and not the same build, via a hard link so
                // `<name>.wasm` is never missing: link the current file to a
                // temp name, rename that over `.prev`, then rename the new
                // file over the current one.
                let keep_current = std::fs::read(&path).is_ok_and(|cur| {
                    format!("{:x}", Sha256::digest(&cur)) != new_sha && validator(&cur).is_ok()
                });
                if keep_current {
                    let keep = dir.join(format!(
                        ".{name}.{}.keep",
                        TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    ));
                    std::fs::hard_link(&path, &keep)
                        .or_else(|_| std::fs::copy(&path, &keep).map(|_| ()))?;
                    std::fs::rename(&keep, &prev)?;
                }
                std::fs::rename(&tmp, &path)
            })
        })
        .await
        .unwrap_or_else(|e| Err(std::io::Error::other(e)))
    };
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("writing {}: {e}\n", path.display()),
        )
            .into_response();
    }
    s.runtime.request_reload();
    tracing::info!(name = %q.name, size = body.len(), "sniffer module uploaded via admin API");
    (
        StatusCode::OK,
        format!("uploaded {}.wasm ({} bytes)\n", q.name, body.len()),
    )
        .into_response()
}

/// `DELETE /admin/sniffers/{name}` — removes a `.wasm` module from
/// `settings.sniffers.dir` and requests a reload so the next rescan drops it
/// from the live registry.
async fn delete_sniffer(State(s): State<AdminState>, Path(name): Path<String>) -> Response {
    let Some(dir) = &s.sniffers_dir else {
        return sniffers_disabled();
    };
    if !valid_module_name(&name) {
        return (StatusCode::BAD_REQUEST, "invalid module name\n".to_string()).into_response();
    }
    let path = dir.join(format!("{name}.wasm"));
    let removed = {
        let _guard = SWAP_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let r = std::fs::remove_file(&path);
        if r.is_ok() {
            match std::fs::remove_file(prev_path(dir, &name)) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    s.runtime.request_reload();
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!(
                            "removed {name}.wasm but not its previous version: {e}; retry the delete\n"
                        ),
                    )
                        .into_response();
                }
                _ => {}
            }
        }
        r
    };
    match removed {
        Ok(()) => {
            s.runtime.request_reload();
            tracing::info!(%name, "sniffer module removed via admin API");
            (StatusCode::OK, format!("removed {name}.wasm\n")).into_response()
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            (StatusCode::NOT_FOUND, "no such module\n".to_string()).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("removing {}: {e}\n", path.display()),
        )
            .into_response(),
    }
}

/// `POST /admin/sniffers/{name}/rollback` — swaps `<name>.wasm` with the kept
/// previous version (so a second rollback undoes the first; exactly one
/// previous is ever kept). `404` without a previous version. On a pinned
/// instance only when the previous version's sha256 equals the pin (`409
/// pinned:`), because anything else would make the next rescan reject the file.
async fn rollback_sniffer(State(s): State<AdminState>, Path(name): Path<String>) -> Response {
    let Some(dir) = &s.sniffers_dir else {
        return sniffers_disabled();
    };
    if !valid_module_name(&name) {
        return (StatusCode::BAD_REQUEST, "invalid module name\n".to_string()).into_response();
    }
    let path = dir.join(format!("{name}.wasm"));
    let prev = prev_path(dir, &name);
    let swapped = {
        let _guard = SWAP_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(prev_sha) = sha_of_file(&prev) else {
            return (
                StatusCode::NOT_FOUND,
                "no previous version kept\n".to_string(),
            )
                .into_response();
        };
        let pins = (s.sniffer_pins)();
        if !pins.is_empty() && !pins.iter().any(|p| p.name == name && p.sha256 == prev_sha) {
            return (
                StatusCode::CONFLICT,
                format!("pinned: the previous version of {name} (sha256 {prev_sha}) does not match its pin\n"),
            )
                .into_response();
        }
        // current -> temp link, prev -> current, temp -> prev; the current
        // file is replaced atomically and never missing.
        let tmp = dir.join(format!(
            ".{name}.{}.swap",
            TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let had_current = path.is_file();
        let r = (|| {
            if had_current {
                std::fs::hard_link(&path, &tmp)
                    .or_else(|_| std::fs::copy(&path, &tmp).map(|_| ()))?;
            }
            std::fs::rename(&prev, &path)?;
            if had_current {
                std::fs::rename(&tmp, &prev)?;
            }
            Ok::<(), std::io::Error>(())
        })();
        match r {
            Err(_) if tmp.exists() && !prev.exists() => {
                // Only the last rename failed: retry it. If it lands the swap
                // is complete (report success); otherwise the former current
                // stays staged under its temp name.
                std::fs::rename(&tmp, &prev)
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                Err(e)
            }
            ok => ok,
        }
    };
    match swapped {
        Ok(()) => {
            s.runtime.request_reload();
            tracing::info!(%name, "sniffer module rolled back via admin API");
            (StatusCode::OK, format!("rolled back {name}.wasm\n")).into_response()
        }
        Err(e) => {
            // The files may have changed before the failure; let the next scan see them.
            s.runtime.request_reload();
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("rolling back {}: {e}\n", path.display()),
            )
                .into_response()
        }
    }
}

fn sniffers_disabled() -> Response {
    (
        StatusCode::CONFLICT,
        "settings.sniffers is not configured on this instance; turning it on from nothing needs \
         a restart, see docs/05-configuration.md\n",
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use metrics_exporter_prometheus::PrometheusBuilder;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use wayhouse_core::{Runtime, Snapshot};

    /// Spin up `admin::router` on an ephemeral port against a live `Runtime`.
    /// Returns the base URL and the runtime (kept alive for the test).
    async fn spawn_admin(yaml: &str) -> (String, Runtime) {
        spawn_admin_with_token(yaml, None).await
    }

    async fn spawn_admin_with_token(yaml: &str, auth_token: Option<&str>) -> (String, Runtime) {
        spawn_admin_full(yaml, auth_token, None).await
    }

    async fn spawn_admin_full(
        yaml: &str,
        auth_token: Option<&str>,
        sniffers_dir: Option<std::path::PathBuf>,
    ) -> (String, Runtime) {
        let cfg = wayhouse_config::parse_str(yaml).unwrap();
        let runtime = Runtime::start(Snapshot::from_config(&cfg), std::sync::Arc::default(), 1);
        let prometheus = PrometheusBuilder::new().build_recorder().handle();
        let app = router(AdminState {
            runtime: runtime.handle(),
            prometheus,
            auth_token: auth_token.map(str::to_string),
            sniffers_dir,
            sniffers: std::sync::Arc::new(wayhouse_core::sniff::Sniffers::default()),
            sniffer_pins: std::sync::Arc::new(Vec::new),
            sniffer_validator: None,
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        tokio::time::sleep(Duration::from_millis(150)).await;
        (format!("http://{addr}"), runtime)
    }

    /// A TCP backend that echoes after a delay, so a proxied connection stays
    /// live long enough to be observed in `GET /sessions`.
    async fn slow_echo_backend() -> SocketAddr {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 64];
                    while let Ok(n) = s.read(&mut buf).await {
                        if n == 0 {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        if s.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        addr
    }

    fn free_port() -> SocketAddr {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
    }

    #[tokio::test]
    async fn healthz_readyz_pools_and_config_respond() {
        let backend = slow_echo_backend().await;
        let proxy = free_port();
        let yaml = format!(
            "pools:\n  - name: p\n    targets: [\"{backend}\"]\n\
             listeners:\n  - name: l\n    bind: \"{proxy}\"\n    pool: p\n"
        );
        let (base, runtime) = spawn_admin(&yaml).await;
        let http = reqwest::Client::new();

        let r = http.get(format!("{base}/healthz")).send().await.unwrap();
        assert!(r.status().is_success());
        assert_eq!(r.text().await.unwrap(), "ok");

        let r = http.get(format!("{base}/readyz")).send().await.unwrap();
        assert!(r.status().is_success());

        let body = http
            .get(format!("{base}/pools"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(body.contains("p\tbalancer="), "pools body: {body}");
        assert!(body.contains(&backend.to_string()));

        let body = http
            .get(format!("{base}/config"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(body.contains("listeners:"), "config body: {body}");
        assert!(body.contains("\tbind=") && body.contains("pools:"));

        runtime
            .shutdown_with_grace(Duration::from_millis(100))
            .await;
    }

    #[tokio::test]
    async fn sessions_endpoint_lists_a_live_connection_and_honours_filters() {
        let backend = slow_echo_backend().await;
        let proxy = free_port();
        let yaml = format!(
            "pools:\n  - name: p\n    targets: [\"{backend}\"]\n\
             listeners:\n  - name: l\n    bind: \"{proxy}\"\n    pool: p\n"
        );
        let (base, runtime) = spawn_admin(&yaml).await;
        let http = reqwest::Client::new();

        let empty = http
            .get(format!("{base}/sessions"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(empty, "sessions=0\n");

        let mut c = TcpStream::connect(proxy).await.unwrap();
        c.write_all(b"ping").await.unwrap();
        let src_port = c.local_addr().unwrap().port();
        tokio::time::sleep(Duration::from_millis(100)).await;

        let body = http
            .get(format!("{base}/sessions"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(body.starts_with("sessions=1\n"), "body: {body}");
        assert!(body.contains("\ttcp\tl\t"), "body: {body}");
        assert!(body.contains("pool=p"), "body: {body}");
        assert!(body.contains(&format!("backend={backend}")), "body: {body}");
        assert!(body.contains(&format!("peer=127.0.0.1:{src_port}")));

        // Filters that should still match.
        for q in ["?proto=tcp", "?listener=l", "?pool=p", "?src=127.0.0.1"] {
            let b = http
                .get(format!("{base}/sessions{q}"))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap();
            assert!(b.starts_with("sessions=1\n"), "{q} -> {b}");
        }
        // Filters that should exclude the session.
        for q in [
            "?proto=udp",
            "?listener=other",
            "?pool=other",
            "?src=10.0.0.1",
        ] {
            let b = http
                .get(format!("{base}/sessions{q}"))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap();
            assert_eq!(b, "sessions=0\n", "{q}");
        }

        drop(c);
        runtime
            .shutdown_with_grace(Duration::from_millis(100))
            .await;
    }

    #[tokio::test]
    async fn auth_token_gates_everything_but_healthz() {
        let yaml = "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
                    listeners:\n  - name: l\n    bind: \"127.0.0.1:0\"\n    pool: p\n";
        let (base, _runtime) = spawn_admin_with_token(yaml, Some("secret123")).await;
        let http = reqwest::Client::new();

        // healthz stays open even with a token configured.
        let r = http.get(format!("{base}/healthz")).send().await.unwrap();
        assert!(r.status().is_success());

        // Everything else is gated: no token, wrong token, right token.
        let r = http.get(format!("{base}/pools")).send().await.unwrap();
        assert_eq!(r.status(), reqwest::StatusCode::UNAUTHORIZED);

        let r = http
            .get(format!("{base}/pools"))
            .bearer_auth("wrong")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), reqwest::StatusCode::UNAUTHORIZED);

        let r = http
            .get(format!("{base}/pools"))
            .bearer_auth("secret123")
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success());
    }

    #[tokio::test]
    async fn sniffer_routes_are_409_when_settings_sniffers_is_absent() {
        let yaml = "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
                    listeners:\n  - name: l\n    bind: \"127.0.0.1:0\"\n    pool: p\n";
        let (base, _runtime) = spawn_admin(yaml).await;
        let http = reqwest::Client::new();

        let r = http
            .get(format!("{base}/admin/sniffers"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), reqwest::StatusCode::CONFLICT);

        let r = http
            .post(format!("{base}/admin/sniffers?name=x"))
            .body(vec![0u8])
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), reqwest::StatusCode::CONFLICT);

        let r = http
            .delete(format!("{base}/admin/sniffers/x"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), reqwest::StatusCode::CONFLICT);
    }

    #[cfg(feature = "wasm-sniffers")]
    mod sniffer_upload {
        use super::*;
        use std::sync::{Arc, Mutex};
        use wayhouse_config::SnifferModulePin;

        const YAML: &str = "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
                            listeners:\n  - name: l\n    bind: \"127.0.0.1:0\"\n    pool: p\n";

        const MODULE_WAT: &str = r#"(module
          (@custom "wayhouse.abi" "\00\00\01\00")
          (memory (export "memory") 1)
          (func (export "alloc") (param i32) (result i32) (i32.const 0))
          (func (export "sniff") (param i32 i32 i32 i32) (result i64) (i64.const 0)))"#;

        fn module() -> Vec<u8> {
            wat::parse_str(MODULE_WAT).unwrap()
        }

        /// A valid module padded with a custom section to `total` bytes.
        fn module_padded_to(total: usize) -> Vec<u8> {
            let mut m = module();
            let name = b"pad";
            let payload = total - m.len() - 1 - 5 - 1 - name.len();
            // section id 0, LEB128 size (5 bytes), name len, name, payload
            let size = 1 + name.len() + payload;
            m.push(0);
            let mut n = size as u32;
            for k in 0..5 {
                let mut b = (n & 0x7f) as u8;
                n >>= 7;
                if k < 4 {
                    b |= 0x80;
                }
                m.push(b);
            }
            m.push(name.len() as u8);
            m.extend_from_slice(name);
            m.extend(std::iter::repeat_n(0u8, payload));
            assert_eq!(m.len(), total);
            m
        }

        fn scratch(tag: &str) -> std::path::PathBuf {
            let dir = std::env::temp_dir().join(format!(
                "wayhouse-admin-sniffer-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        async fn spawn(
            dir: &std::path::Path,
            pins: Arc<Mutex<Vec<SnifferModulePin>>>,
        ) -> (String, Runtime) {
            let cfg = wayhouse_config::parse_str(YAML).unwrap();
            let runtime = Runtime::start(Snapshot::from_config(&cfg), std::sync::Arc::default(), 1);
            let loader = Arc::new(
                crate::sniffer_loader::SnifferLoader::new(Duration::from_millis(50)).unwrap(),
            );
            let app = router(AdminState {
                runtime: runtime.handle(),
                prometheus: PrometheusBuilder::new().build_recorder().handle(),
                auth_token: None,
                sniffers_dir: Some(dir.to_path_buf()),
                sniffers: Arc::new(wayhouse_core::sniff::Sniffers::default()),
                sniffer_pins: Arc::new(move || pins.lock().unwrap().clone()),
                sniffer_validator: Some(Arc::new(move |b: &[u8]| {
                    loader.validate(b, 1 << 20).map_err(|e| e.to_string())
                })),
            });
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            tokio::time::sleep(Duration::from_millis(150)).await;
            (format!("http://{addr}"), runtime)
        }

        async fn post(base: &str, name: &str, body: Vec<u8>) -> reqwest::Response {
            reqwest::Client::new()
                .post(format!("{base}/admin/sniffers?name={name}"))
                .body(body)
                .send()
                .await
                .unwrap()
        }

        fn no_pins() -> Arc<Mutex<Vec<SnifferModulePin>>> {
            Arc::default()
        }

        fn pin(name: &str, bytes: &[u8]) -> SnifferModulePin {
            SnifferModulePin {
                name: name.into(),
                sha256: format!("{:x}", Sha256::digest(bytes)),
                config: None,
            }
        }

        fn dir_is_empty(dir: &std::path::Path) -> bool {
            std::fs::read_dir(dir).unwrap().next().is_none()
        }

        #[tokio::test]
        async fn upload_list_and_delete_a_sniffer_module_round_trips() {
            let dir = scratch("roundtrip");
            let (base, _rt) = spawn(&dir, no_pins()).await;
            let http = reqwest::Client::new();
            let list = |base: String| async move {
                reqwest::get(format!("{base}/admin/sniffers"))
                    .await
                    .unwrap()
                    .json::<Vec<SnifferInfo>>()
                    .await
                    .unwrap()
            };
            assert!(list(base.clone()).await.is_empty());

            // Reject a path-traversal-shaped name before touching the filesystem.
            let r = post(&base, "..%2Fevil", module()).await;
            assert_eq!(r.status(), reqwest::StatusCode::BAD_REQUEST);

            let bytes = module();
            let r = post(&base, "demo", bytes.clone()).await;
            assert!(r.status().is_success());
            assert!(dir.join("demo.wasm").is_file());
            let l = list(base.clone()).await;
            assert_eq!(l.len(), 1);
            assert_eq!(l[0].name, "demo");
            assert_eq!(l[0].size_bytes, bytes.len() as u64);

            let r = http
                .delete(format!("{base}/admin/sniffers/demo"))
                .send()
                .await
                .unwrap();
            assert!(r.status().is_success());
            assert!(!dir.join("demo.wasm").exists());
            let r = http
                .delete(format!("{base}/admin/sniffers/demo"))
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), reqwest::StatusCode::NOT_FOUND);
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[tokio::test]
        async fn upload_rejects_garbage_with_400_and_writes_nothing() {
            let dir = scratch("garbage");
            let (base, _rt) = spawn(&dir, no_pins()).await;
            let r = post(&base, "junk", b"not wasm".to_vec()).await;
            assert_eq!(r.status(), reqwest::StatusCode::BAD_REQUEST);
            assert!(r.text().await.unwrap().contains("invalid sniffer module"));
            assert!(dir_is_empty(&dir));
        }

        #[tokio::test]
        async fn upload_rejects_truncated_module_with_400() {
            let dir = scratch("truncated");
            let (base, _rt) = spawn(&dir, no_pins()).await;
            let mut m = module();
            m.truncate(m.len() - 3);
            let r = post(&base, "cut", m).await;
            assert_eq!(r.status(), reqwest::StatusCode::BAD_REQUEST);
            assert!(dir_is_empty(&dir));
        }

        #[tokio::test]
        async fn upload_rejects_empty_body() {
            let dir = scratch("empty");
            let (base, _rt) = spawn(&dir, no_pins()).await;
            let r = post(&base, "nothing", Vec::new()).await;
            assert_eq!(r.status(), reqwest::StatusCode::BAD_REQUEST);
            assert!(dir_is_empty(&dir));
        }

        #[tokio::test]
        async fn upload_rejects_oversize_with_413() {
            let dir = scratch("oversize");
            let (base, _rt) = spawn(&dir, no_pins()).await;
            let r = post(
                &base,
                "big",
                vec![0u8; crate::sniffer_loader::MAX_MODULE_BYTES + 1],
            )
            .await;
            assert_eq!(r.status(), reqwest::StatusCode::PAYLOAD_TOO_LARGE);
            assert!(dir_is_empty(&dir));
        }

        #[tokio::test]
        async fn upload_accepts_valid_module_larger_than_2mib() {
            let dir = scratch("big-ok");
            let (base, _rt) = spawn(&dir, no_pins()).await;
            let r = post(&base, "big", module_padded_to(3 << 20)).await;
            let st = r.status();
            assert!(st.is_success(), "{st} {}", r.text().await.unwrap());
            assert_eq!(
                std::fs::metadata(dir.join("big.wasm")).unwrap().len(),
                3 << 20
            );
        }

        #[tokio::test]
        async fn upload_writes_via_rename_and_leaves_no_tmp_file() {
            let dir = scratch("atomic");
            let (base, _rt) = spawn(&dir, no_pins()).await;
            let (a, b) = tokio::join!(
                post(&base, "same", module_padded_to(1 << 20)),
                post(&base, "same", module_padded_to(2 << 20)),
            );
            assert!(a.status().is_success() && b.status().is_success());
            let names: Vec<_> = std::fs::read_dir(&dir)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            assert!(!names.iter().any(|n| n.ends_with(".tmp")), "{names:?}");
            assert!(names.contains(&"same.wasm".to_string()), "{names:?}");
            let len = std::fs::metadata(dir.join("same.wasm")).unwrap().len();
            assert!(len == 1 << 20 || len == 2 << 20, "whole file, got {len}");
        }

        async fn rollback(base: &str, name: &str) -> reqwest::Response {
            reqwest::Client::new()
                .post(format!("{base}/admin/sniffers/{name}/rollback"))
                .send()
                .await
                .unwrap()
        }

        async fn list(base: &str) -> Vec<SnifferInfo> {
            reqwest::get(format!("{base}/admin/sniffers"))
                .await
                .unwrap()
                .json()
                .await
                .unwrap()
        }

        fn len_of(dir: &std::path::Path, file: &str) -> u64 {
            std::fs::metadata(dir.join(file)).unwrap().len()
        }

        #[tokio::test]
        async fn upload_keeps_the_replaced_module_as_prev_and_lists_it() {
            let dir = scratch("keep-prev");
            let (base, _rt) = spawn(&dir, no_pins()).await;
            assert!(post(&base, "demo", module_padded_to(1 << 20))
                .await
                .status()
                .is_success());
            assert!(
                !list(&base).await[0].has_previous,
                "first install has no prev"
            );
            assert!(post(&base, "demo", module_padded_to(2 << 20))
                .await
                .status()
                .is_success());
            assert_eq!(len_of(&dir, ".demo.wasm.prev"), 1 << 20);
            assert_eq!(len_of(&dir, "demo.wasm"), 2 << 20);
            let l = list(&base).await;
            assert_eq!(l.len(), 1, "the .prev file is not a module of its own");
            assert!(l[0].has_previous);
            assert!(!l[0].fallback);
        }

        #[tokio::test]
        async fn invalid_current_never_replaces_the_known_good_prev() {
            let dir = scratch("bad-current");
            std::fs::write(dir.join("demo.wasm"), b"not wasm").unwrap();
            std::fs::write(dir.join(".demo.wasm.prev"), module_padded_to(1 << 20)).unwrap();
            let (base, _rt) = spawn(&dir, no_pins()).await;
            assert!(post(&base, "demo", module_padded_to(2 << 20))
                .await
                .status()
                .is_success());
            assert_eq!(len_of(&dir, ".demo.wasm.prev"), 1 << 20, "good prev kept");
            assert_eq!(len_of(&dir, "demo.wasm"), 2 << 20);
        }

        #[tokio::test]
        async fn concurrent_first_uploads_still_keep_one_as_prev() {
            let dir = scratch("concurrent-new");
            let (base, _rt) = spawn(&dir, no_pins()).await;
            let (a, b) = tokio::join!(
                post(&base, "demo", module_padded_to(1 << 20)),
                post(&base, "demo", module_padded_to(2 << 20)),
            );
            assert!(a.status().is_success() && b.status().is_success());
            assert!(
                dir.join(".demo.wasm.prev").is_file(),
                "the first install is rollbackable"
            );
        }

        #[tokio::test]
        async fn identical_reupload_does_not_overwrite_prev() {
            let dir = scratch("same-sha");
            let (base, _rt) = spawn(&dir, no_pins()).await;
            for n in [1, 2, 2] {
                assert!(post(&base, "demo", module_padded_to(n << 20))
                    .await
                    .status()
                    .is_success());
            }
            assert_eq!(len_of(&dir, ".demo.wasm.prev"), 1 << 20, "prev stays v1");
        }

        #[tokio::test]
        async fn rollback_swaps_current_and_prev() {
            let dir = scratch("rollback");
            let (base, _rt) = spawn(&dir, no_pins()).await;
            for n in [1, 2, 3] {
                assert!(post(&base, "demo", module_padded_to(n << 20))
                    .await
                    .status()
                    .is_success());
            }
            // Two updates, then a rollback: the version before the last update.
            let r = rollback(&base, "demo").await;
            assert!(r.status().is_success(), "{}", r.status());
            assert_eq!(len_of(&dir, "demo.wasm"), 2 << 20);
            assert_eq!(
                len_of(&dir, ".demo.wasm.prev"),
                3 << 20,
                "swap keeps exactly one"
            );
            let names: Vec<_> = std::fs::read_dir(&dir)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            assert_eq!(names.len(), 2, "{names:?}");
        }

        #[tokio::test]
        async fn rollback_without_prev_is_404_and_bad_name_is_400() {
            let dir = scratch("rollback-none");
            let (base, _rt) = spawn(&dir, no_pins()).await;
            assert!(post(&base, "demo", module()).await.status().is_success());
            assert_eq!(
                rollback(&base, "demo").await.status(),
                reqwest::StatusCode::NOT_FOUND
            );
            assert_eq!(
                rollback(&base, "ghost").await.status(),
                reqwest::StatusCode::NOT_FOUND
            );
            assert_eq!(
                rollback(&base, "a.b").await.status(),
                reqwest::StatusCode::BAD_REQUEST
            );
        }

        #[tokio::test]
        async fn delete_removes_prev_too() {
            let dir = scratch("delete-prev");
            let (base, _rt) = spawn(&dir, no_pins()).await;
            for n in [1, 2] {
                assert!(post(&base, "demo", module_padded_to(n << 20))
                    .await
                    .status()
                    .is_success());
            }
            let r = reqwest::Client::new()
                .delete(format!("{base}/admin/sniffers/demo"))
                .send()
                .await
                .unwrap();
            assert!(r.status().is_success());
            assert!(dir_is_empty(&dir));
        }

        #[tokio::test]
        async fn pinned_rollback_only_when_prev_matches_the_pin() {
            let dir = scratch("rollback-pin");
            let v1 = module_padded_to(1 << 20);
            let v2 = module_padded_to(2 << 20);
            // Pinned to v2: prev (v1) differs, so rollback is refused with `pinned:`.
            std::fs::write(dir.join("demo.wasm"), &v2).unwrap();
            std::fs::write(dir.join(".demo.wasm.prev"), &v1).unwrap();
            let pins = Arc::new(Mutex::new(vec![pin("demo", &v2)]));
            let (base, _rt) = spawn(&dir, pins).await;
            let r = rollback(&base, "demo").await;
            assert_eq!(r.status(), reqwest::StatusCode::CONFLICT);
            assert!(r.text().await.unwrap().starts_with("pinned:"));
            assert_eq!(len_of(&dir, "demo.wasm"), 2 << 20, "nothing changed");
            // Pinned to v1 while v2 is current: rollback to v1 is allowed.
            let dir2 = scratch("rollback-pin-ok");
            std::fs::write(dir2.join("demo.wasm"), &v2).unwrap();
            std::fs::write(dir2.join(".demo.wasm.prev"), &v1).unwrap();
            let pins = Arc::new(Mutex::new(vec![pin("demo", &v1)]));
            let (base2, _rt2) = spawn(&dir2, pins).await;
            assert!(rollback(&base2, "demo").await.status().is_success());
            assert_eq!(len_of(&dir2, "demo.wasm"), 1 << 20);
        }

        #[tokio::test]
        async fn upload_to_pinned_instance_with_other_hash_is_409_and_writes_nothing() {
            let dir = scratch("pin-mismatch");
            let pins = Arc::new(Mutex::new(vec![pin("demo", b"something else")]));
            let (base, _rt) = spawn(&dir, pins).await;
            let r = post(&base, "demo", module()).await;
            assert_eq!(r.status(), reqwest::StatusCode::CONFLICT);
            assert!(r.text().await.unwrap().contains("pinned"));
            assert!(dir_is_empty(&dir));
        }

        #[tokio::test]
        async fn upload_unpinned_name_on_pinned_instance_is_409_and_writes_nothing() {
            let dir = scratch("pin-unlisted");
            let pins = Arc::new(Mutex::new(vec![pin("other", &module())]));
            let (base, _rt) = spawn(&dir, pins).await;
            let r = post(&base, "demo", module()).await;
            assert_eq!(r.status(), reqwest::StatusCode::CONFLICT);
            assert!(dir_is_empty(&dir));
        }

        #[tokio::test]
        async fn upload_matching_pin_is_accepted() {
            let dir = scratch("pin-ok");
            let pins = Arc::new(Mutex::new(vec![pin("demo", &module())]));
            let (base, _rt) = spawn(&dir, pins).await;
            let r = post(&base, "demo", module()).await;
            assert!(r.status().is_success());
            assert!(dir.join("demo.wasm").is_file());
        }

        #[tokio::test]
        async fn pin_check_uses_live_pins_after_reload() {
            let dir = scratch("pin-live");
            let pins = no_pins();
            let (base, _rt) = spawn(&dir, pins.clone()).await;
            assert!(post(&base, "a", module()).await.status().is_success());
            pins.lock().unwrap().push(pin("zzz", b"x"));
            let r = post(&base, "b", module()).await;
            assert_eq!(r.status(), reqwest::StatusCode::CONFLICT);
            assert!(!dir.join("b.wasm").exists());
        }
    }

    /// The aggregator's fan-out carries the protocol header: an unknown major
    /// is refused with both versions named, while `curl` (no header) still works.
    #[tokio::test]
    async fn admin_routes_refuse_another_protocol_major_but_accept_no_header() {
        let proxy = free_port();
        let yaml = format!(
            "pools:\n  - name: p\n    targets: [\"127.0.0.1:9\"]\n    health_check:\n      type: none\n\
             listeners:\n  - name: l\n    bind: \"{proxy}\"\n    pool: p\n"
        );
        let (base, runtime) = spawn_admin(&yaml).await;
        let http = reqwest::Client::new();
        let get = |header: Option<&'static str>| {
            let mut r = http.get(format!("{base}/pools"));
            if let Some(h) = header {
                r = r.header(wayhouse_http::protocol::HEADER, h);
            }
            r.send()
        };
        assert!(get(None).await.unwrap().status().is_success());
        assert!(get(Some("1.4")).await.unwrap().status().is_success());
        let refused = get(Some("2.0")).await.unwrap();
        assert_eq!(refused.status(), reqwest::StatusCode::UPGRADE_REQUIRED);
        let body = refused.text().await.unwrap();
        assert!(body.contains("2.0") && body.contains("(1.0)"), "{body}");
        // Liveness is never gated.
        let health = http
            .get(format!("{base}/healthz"))
            .header(wayhouse_http::protocol::HEADER, "2.0")
            .send()
            .await
            .unwrap();
        assert!(health.status().is_success());
        runtime
            .shutdown_with_grace(std::time::Duration::from_millis(100))
            .await;
    }
}
