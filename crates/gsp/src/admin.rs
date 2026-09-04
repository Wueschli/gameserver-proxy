//! The admin / observability HTTP API. Bound to an internal address only.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use axum::{
    extract::{Path, Query, State},
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

/// The admin API route table. Split out from [`serve`] so integration tests can
/// mount it on their own ephemeral listener.
fn router(state: AdminState) -> Router {
    Router::new()
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
        .route("/sessions", get(sessions))
        .route("/admin/drain", post(drain))
        .route("/admin/undrain", post(undrain))
        .route("/route-hint", post(route_hint))
        .with_state(state)
}

pub async fn serve(addr: SocketAddr, runtime: RuntimeHandle, prometheus: PrometheusHandle) {
    let app = router(AdminState {
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
        let cfg = gsp_config::parse_str(yaml).unwrap();
        let runtime = Runtime::start(Snapshot::from_config(&cfg), Default::default(), 1);
        let prometheus = PrometheusBuilder::new().build_recorder().handle();
        let app = router(AdminState {
            runtime: runtime.handle(),
            prometheus,
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
}
