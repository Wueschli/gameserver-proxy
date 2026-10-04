//! The Prometheus recorder and `GET /metrics` route the fleet binaries
//! (`gsp-controller`, `gsp-aggregator`, `gsp-ui`) share, so the counters
//! `gsp_http` itself emits (`gsp_tls_handshakes_*`) can be scraped. `gsp`
//! installs its own recorder and serves `/metrics` from its admin API.
//!
//! The route sits behind the same [`BearerAuth`] gate as the rest of a
//! binary's API: no token configured means open, a token means
//! `Authorization: Bearer`. Mount it with `Router::merge`.

use axum::routing::get;
use axum::Router;
use metrics_exporter_prometheus::{BuildError, PrometheusBuilder, PrometheusHandle};

use crate::server::{require_bearer, BearerAuth};

/// Gauge, always `1`. Labels: `component` (the binary), `version`. Set by
/// [`install`], so a scrape of a freshly started binary is never empty.
pub const BUILD_INFO: &str = "gsp_build_info";

/// Install the process-global recorder and set [`BUILD_INFO`]. Call once from
/// `main`; a second call in the same process is an error.
pub fn install(
    component: &'static str,
    version: &'static str,
) -> Result<PrometheusHandle, BuildError> {
    let handle = PrometheusBuilder::new().install_recorder()?;
    metrics::gauge!(BUILD_INFO, "component" => component, "version" => version).set(1.0);
    Ok(handle)
}

/// `GET /metrics`, rendering `handle`, gated by `auth`.
pub fn router(handle: PrometheusHandle, auth: BearerAuth) -> Router {
    Router::new()
        .route("/metrics", get(move || std::future::ready(handle.render())))
        .route_layer(axum::middleware::from_fn_with_state(auth, require_bearer))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use tower::ServiceExt;

    async fn get_metrics(app: Router, authorization: Option<&str>) -> (StatusCode, String) {
        let mut req = Request::builder().uri("/metrics");
        if let Some(v) = authorization {
            req = req.header(header::AUTHORIZATION, v);
        }
        let resp = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    /// A handle whose recorder is not installed globally, so tests don't race.
    fn handle_with_sample() -> PrometheusHandle {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            metrics::counter!("gsp_test_total").increment(3);
        });
        handle
    }

    #[tokio::test]
    async fn serves_the_rendered_metrics() {
        let app = router(handle_with_sample(), BearerAuth::new(None));
        let (status, body) = get_metrics(app, None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("gsp_test_total 3"), "{body}");
    }

    #[tokio::test]
    async fn a_token_gates_the_route() {
        let app = || router(handle_with_sample(), BearerAuth::new(Some("s3cret")));
        assert_eq!(get_metrics(app(), None).await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(
            get_metrics(app(), Some("Bearer nope")).await.0,
            StatusCode::UNAUTHORIZED
        );
        let (status, body) = get_metrics(app(), Some("Bearer s3cret")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("gsp_test_total 3"));
    }
}
