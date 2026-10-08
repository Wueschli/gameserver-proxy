//! The Prometheus recorder and `GET /metrics` route the fleet binaries
//! (`wayhouse-controller`, `wayhouse-aggregator`, `wayhouse-ui`) share, so the counters
//! `wayhouse_http` itself emits (`wayhouse_tls_handshakes_*`) can be scraped. `wayhouse`
//! installs its own recorder and serves `/metrics` from its admin API.
//!
//! The route sits behind the same [`BearerAuth`] gate as the rest of a
//! binary's API: no token configured means open, a token means
//! `Authorization: Bearer`. Mount it with `Router::merge`.

use axum::routing::get;
use axum::Router;
use metrics_exporter_prometheus::{BuildError, PrometheusBuilder, PrometheusHandle};

use crate::server::{require_bearer, BearerAuth};

/// Gauge, always `1`. Labels: `component` (the binary), `version`, `commit`,
/// `protocol` (the wire protocol `major.minor`).
/// The same label set on `wayhouse` and every fleet binary, so one query covers a
/// whole deployment. Set by [`install`] / [`set_build_info`], so a scrape of a
/// freshly started binary is never empty.
pub const BUILD_INFO: &str = "wayhouse_build_info";

pub use crate::COMMIT;

/// Set [`BUILD_INFO`] on whichever recorder is installed. `wayhouse` installs its
/// own recorder and calls this directly; the fleet binaries go through
/// [`install`].
pub fn set_build_info(component: &'static str, version: &'static str) {
    metrics::gauge!(BUILD_INFO, "component" => component, "version" => version, "commit" => COMMIT, "protocol" => crate::protocol::ProtocolVersion::current().to_string())
        .set(1.0);
}

/// Install the process-global recorder and set [`BUILD_INFO`]. Call once from
/// `main`; a second call in the same process is an error.
pub fn install(
    component: &'static str,
    version: &'static str,
) -> Result<PrometheusHandle, BuildError> {
    let handle = PrometheusBuilder::new().install_recorder()?;
    set_build_info(component, version);
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
            metrics::counter!("wayhouse_test_total").increment(3);
        });
        handle
    }

    #[test]
    fn build_info_has_protocol_label() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || set_build_info("wayhouse-x", "1.2.3"));
        let body = handle.render();
        assert!(
            body.contains(&format!(
                "wayhouse_build_info{{component=\"wayhouse-x\",version=\"1.2.3\",commit=\"{COMMIT}\",protocol=\"{}\"}} 1",
                crate::protocol::ProtocolVersion::CURRENT
            )),
            "{body}"
        );
        assert!(!COMMIT.is_empty());
    }

    #[tokio::test]
    async fn serves_the_rendered_metrics() {
        let app = router(handle_with_sample(), BearerAuth::new(None));
        let (status, body) = get_metrics(app, None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("wayhouse_test_total 3"), "{body}");
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
        assert!(body.contains("wayhouse_test_total 3"));
    }
}
