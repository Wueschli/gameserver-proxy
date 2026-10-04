//! The admin / observability HTTP API. Bound to an internal address only.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::StatusCode,
    middleware::{self},
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post},
    Json, Router,
};
use metrics_exporter_prometheus::PrometheusHandle;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use gsp_core::pool::AdminState as BackendState;
use gsp_core::sniff::Sniffers;
use gsp_core::RuntimeHandle;

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
}

/// The admin API route table. Split out from [`serve`] so integration tests can
/// mount it on their own ephemeral listener. `/healthz` alone stays outside
/// the [`require_bearer`] gate — plain liveness-probe convention, same
/// choice `gsp-controller`/`gsp-aggregator` made for their own `/healthz`.
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
        .route("/admin/sniffers", get(list_sniffers).post(upload_sniffer))
        .route("/admin/sniffers/{name}", delete(delete_sniffer))
        .route_layer(middleware::from_fn_with_state(
            gsp_http::server::BearerAuth::new(state.auth_token.as_deref())
                .with_body("unauthorized\n"),
            gsp_http::server::require_bearer,
        ));

    Router::new()
        .route("/healthz", get(healthz))
        .merge(gated)
        .with_state(state)
}

pub async fn serve(
    addr: SocketAddr,
    tls: Option<std::sync::Arc<gsp_http::tls::ReloadingCert>>,
    runtime: RuntimeHandle,
    prometheus: PrometheusHandle,
    auth_token: Option<String>,
    sniffers_dir: Option<PathBuf>,
    sniffers: std::sync::Arc<Sniffers>,
) {
    let app = router(AdminState {
        runtime,
        prometheus,
        auth_token,
        sniffers_dir,
        sniffers,
    });

    // HTTPS with `settings.admin.tls`, plain HTTP otherwise. A bind failure
    // lands here too and, as before, ends only this task (non-fatal).
    if let Err(e) = gsp_http::tls::serve(addr, app, tls, "admin API").await {
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
    let snap = s.runtime.snapshot();
    let mut out = String::new();
    fn opt(v: Option<impl std::fmt::Display>) -> String {
        v.map_or_else(|| "-".to_string(), |n| n.to_string())
    }
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
                    let f = |b: Option<gsp_config::TokenBucket>| match b {
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
            match &l.sniffer {
                Some(n) => format!("\tsniffer={n}"),
                None => String::new(),
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
            name,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Json(out).into_response()
}

/// `POST /admin/sniffers?name=<module>` — uploads a `.wasm` module's raw
/// bytes into `settings.sniffers.dir/<name>.wasm`, then requests a reload so
/// `SnifferLoader::scan` (already rescanned live on every reload, phase 9
/// slice 4) picks it up — the same hot mechanism `POST /pools/{pool}/backends`
/// uses for the runtime overlay, not a second one. If `settings.sniffers.
/// modules` pins hashes on this instance, an upload under an unpinned name
/// loads the file but the *next* scan then rejects the whole registry
/// update per the existing pin-enforcement rule in `sniffer_loader.rs` —
/// updating the pin list itself is a config change, out of scope for this
/// endpoint.
#[derive(Deserialize)]
struct SnifferUploadQuery {
    name: String,
}

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
    let path = dir.join(format!("{}.wasm", q.name));
    if let Err(e) = std::fs::write(&path, &body) {
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
    match std::fs::remove_file(&path) {
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

    use gsp_core::{Runtime, Snapshot};
    use metrics_exporter_prometheus::PrometheusBuilder;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

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
        let cfg = gsp_config::parse_str(yaml).unwrap();
        let runtime = Runtime::start(Snapshot::from_config(&cfg), Default::default(), 1);
        let prometheus = PrometheusBuilder::new().build_recorder().handle();
        let app = router(AdminState {
            runtime: runtime.handle(),
            prometheus,
            auth_token: auth_token.map(str::to_string),
            sniffers_dir,
            sniffers: std::sync::Arc::new(gsp_core::sniff::Sniffers::default()),
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

    #[tokio::test]
    async fn upload_list_and_delete_a_sniffer_module_round_trips() {
        let dir = std::env::temp_dir().join(format!(
            "gsp-admin-sniffer-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let yaml = "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
                    listeners:\n  - name: l\n    bind: \"127.0.0.1:0\"\n    pool: p\n";
        let (base, _runtime) = spawn_admin_full(yaml, None, Some(dir.clone())).await;
        let http = reqwest::Client::new();

        // Nothing uploaded yet.
        let list: Vec<SnifferInfo> = http
            .get(format!("{base}/admin/sniffers"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(list.is_empty());

        // Reject a path-traversal-shaped name before touching the filesystem.
        let r = http
            .post(format!("{base}/admin/sniffers?name=../evil"))
            .body(vec![1u8, 2, 3])
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), reqwest::StatusCode::BAD_REQUEST);

        // A real upload lands on disk and shows up in the listing.
        let bytes = vec![0u8, 1, 2, 3, 4];
        let r = http
            .post(format!("{base}/admin/sniffers?name=demo"))
            .body(bytes.clone())
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success());
        assert!(dir.join("demo.wasm").is_file());

        let list: Vec<SnifferInfo> = http
            .get(format!("{base}/admin/sniffers"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "demo");
        assert_eq!(list[0].size_bytes, bytes.len() as u64);
        // Not a real wasm module, so the registry never actually loaded it —
        // `loaded` reports the live registry's state, not just file presence.
        assert!(!list[0].loaded);

        // Delete removes the file.
        let r = http
            .delete(format!("{base}/admin/sniffers/demo"))
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success());
        assert!(!dir.join("demo.wasm").exists());

        // Deleting again is a clean 404, not a panic.
        let r = http
            .delete(format!("{base}/admin/sniffers/demo"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), reqwest::StatusCode::NOT_FOUND);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
