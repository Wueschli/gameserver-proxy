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
        .route("/pools/{pool}/backends/{addr}", patch(patch_backend))
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
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not ready")
    }
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
