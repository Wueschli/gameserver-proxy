//! The admin / observability HTTP API. Bound to an internal address only.

use std::net::SocketAddr;

use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::get, Router};
use metrics_exporter_prometheus::PrometheusHandle;

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

async fn pools(State(s): State<AdminState>) -> impl IntoResponse {
    let snap = s.runtime.snapshot();
    let mut out = String::new();
    for (name, pool) in &snap.pools {
        out.push_str(&format!(
            "{name}\tbalancer={:?}\ttargets={}\n",
            pool.balancer,
            pool.targets().len()
        ));
    }
    out
}
