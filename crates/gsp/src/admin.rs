//! The admin / observability HTTP API. Bound to an internal address only.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use metrics_exporter_prometheus::PrometheusHandle;
use serde::Deserialize;

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
                "  {}\t{}\tactive={}\n",
                b.addr,
                if b.is_healthy() {
                    "healthy"
                } else {
                    "unhealthy"
                },
                b.active(),
            ));
        }
    }
    out
}
