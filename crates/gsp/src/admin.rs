//! The admin / observability HTTP API. Bound to an internal address only.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, patch, post},
    Json, Router,
};
use metrics_exporter_prometheus::PrometheusHandle;
use serde::Deserialize;

use gsp_core::pool::AdminState as BackendState;
use gsp_core::RuntimeHandle;

#[derive(Clone)]
struct AdminState {
    runtime: RuntimeHandle,
    prometheus: PrometheusHandle,
}

pub async fn serve(addr: SocketAddr, runtime: RuntimeHandle, prometheus: PrometheusHandle) {
    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics))
        .route("/pools", get(pools))
        .route("/pools/{pool}/backends", post(add_backend))
        .route(
            "/pools/{pool}/backends/{addr}",
            patch(patch_backend).delete(delete_backend),
        )
        .route("/config", get(config))
        .route("/admin/drain", post(drain))
        .route("/admin/undrain", post(undrain))
        .route("/route-hint", post(route_hint))
        .with_state(AdminState {
            runtime,
            prometheus,
        });

    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(%addr, error = %e, "failed to bind admin listener");
            return;
        }
    };
    tracing::info!(%addr, "admin API listening");
    if let Err(e) = axum::serve(listener, app).await {
        tracing::error!(error = %e, "admin API server error");
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
        out.push_str(&format!(
            "  {}\tbind={}\tproto={:?}\troutes={}{}{}{}{}{}{}{}{}{}\n",
            l.name,
            l.bind,
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
