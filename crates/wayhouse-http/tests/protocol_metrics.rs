//! A refused request is counted per route group. One test per binary: it installs
//! the process-global metrics recorder.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use axum::Router;
use tower::ServiceExt;
use wayhouse_http::protocol::{gate, HEADER};

#[tokio::test]
async fn a_refused_request_increments_the_mismatch_counter() {
    let prom = metrics_exporter_prometheus::PrometheusBuilder::new()
        .install_recorder()
        .unwrap();
    let app = gate(
        Router::new().route("/x", get(|| async { "ok" })),
        "controller",
    );
    for header in ["2.0", "banana"] {
        let resp = app
            .clone()
            .oneshot(
                Request::get("/x")
                    .header(HEADER, header)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UPGRADE_REQUIRED);
    }
    let ok = app
        .oneshot(
            Request::get("/x")
                .header(HEADER, "1.3")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::OK);

    let out = prom.render();
    assert!(
        out.contains("wayhouse_protocol_mismatch_total{route_group=\"controller\"} 2"),
        "{out}"
    );
}
