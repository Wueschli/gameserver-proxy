//! Shared helper for `crate::aggregator_proxy` and `crate::controller_proxy`
//! — both are thin pass-through proxies to a machine service, and both need
//! to forward the upstream response's headers faithfully, not just its
//! status and body.

/// The upstream response's headers, minus the hop-by-hop ones that don't
/// make sense to forward verbatim (`axum` recomputes `content-length` for
/// the body we actually send; `connection`/`transfer-encoding` describe
/// *this* hop's framing, not the content). Everything else — notably
/// `wayhouse-controller`'s `X-Config-Revision` — passes through, so a caller
/// reading a response header from `wayhouse-ui` sees exactly what the upstream
/// service sent.
pub fn forwardable_headers(upstream: &axum::http::HeaderMap) -> axum::http::HeaderMap {
    let mut headers = upstream.clone();
    for h in [
        axum::http::header::CONNECTION,
        axum::http::header::TRANSFER_ENCODING,
        axum::http::header::CONTENT_LENGTH,
    ] {
        headers.remove(h);
    }
    headers
}

/// Wire-shape contract for *every* route `wayhouse-ui` proxies: whatever the
/// upstream service answers — status, custom headers — must reach the
/// browser unchanged, and the hop-by-hop framing headers must not. This bug
/// class (a pass-through proxy silently dropping response headers) has hit
/// this codebase more than once, and status + body assertions don't catch
/// it, so the check is table-driven over the whole proxied surface.
///
/// **When you add a proxied route, add it to `ROUTES`.** (axum can't
/// enumerate a router's routes, so the table is maintained by hand.)
#[cfg(test)]
mod header_contract {
    use axum::body::Body;
    use axum::http::{header, Method, Request, StatusCode};
    use axum::routing::any;
    use axum::Router;
    use tower::ServiceExt;

    /// `(method, browser-facing path)` — one entry per proxied route.
    const ROUTES: &[(&str, &str)] = &[
        // aggregator_proxy: viewer reads
        ("GET", "/api/fleet/pools"),
        ("GET", "/api/fleet/sessions"),
        ("GET", "/api/fleet/healthz"),
        ("GET", "/api/fleet/instances/fra-1/sniffers"),
        // aggregator_proxy: operator verbs
        ("POST", "/api/fleet/instances/fra-1/drain"),
        ("POST", "/api/fleet/instances/fra-1/undrain"),
        ("POST", "/api/fleet/pools/lobby/backends"),
        ("PATCH", "/api/fleet/pools/lobby/backends/10.0.0.5:7777"),
        ("DELETE", "/api/fleet/pools/lobby/backends/10.0.0.5:7777"),
        ("POST", "/api/fleet/route-hint"),
        ("POST", "/api/fleet/sniffers?name=a2s.wasm"),
        ("DELETE", "/api/fleet/sniffers/a2s.wasm"),
        // controller_proxy: viewer reads
        ("GET", "/api/config"),
        ("GET", "/api/config/revisions"),
        ("GET", "/api/config/revisions/1"),
        ("GET", "/api/config/revisions/1/diff?against=2"),
        ("GET", "/api/tunnel/addresses"),
        // controller_proxy: admin writes
        ("POST", "/api/config"),
        ("POST", "/api/config/rollback/1"),
        ("POST", "/api/config/promote/1"),
    ];

    /// An upstream that answers *every* method and path the same way: a
    /// non-200 success status, two end-to-end headers, and a body.
    async fn spawn_upstream() -> String {
        let app = Router::new().fallback(any(|| async {
            (
                StatusCode::ACCEPTED,
                [
                    ("x-config-revision", "42"),
                    ("x-contract-probe", "survives"),
                ],
                "upstream-body",
            )
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn every_proxied_route_forwards_status_and_response_headers() {
        let upstream = spawn_upstream().await;
        let state = crate::api::AppState::new(Some("secret".into()))
            .with_aggregator(upstream.clone(), None)
            .with_controller(upstream, None);
        let session = state.sessions.create(crate::session::Session {
            role: crate::role::Role::Admin,
            username: Some("alice".into()),
        });
        let app = crate::api::router(state);

        for (method, path) in ROUTES {
            let req = Request::builder()
                .method(Method::from_bytes(method.as_bytes()).unwrap())
                .uri(*path)
                .header(
                    header::COOKIE,
                    format!("{}={session}", crate::api::SESSION_COOKIE),
                )
                .body(Body::from("{}"))
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();

            assert_eq!(
                resp.status(),
                StatusCode::ACCEPTED,
                "{method} {path}: status"
            );
            for (name, want) in [
                ("x-config-revision", "42"),
                ("x-contract-probe", "survives"),
            ] {
                assert_eq!(
                    resp.headers().get(name).and_then(|v| v.to_str().ok()),
                    Some(want),
                    "{method} {path}: response header `{name}` was dropped or changed"
                );
            }
            let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            assert_eq!(body, "upstream-body".as_bytes(), "{method} {path}: body");
        }
    }

    #[test]
    fn hop_by_hop_headers_are_stripped_and_the_rest_kept() {
        let mut upstream = axum::http::HeaderMap::new();
        upstream.insert(header::CONNECTION, "close".parse().unwrap());
        upstream.insert(header::TRANSFER_ENCODING, "chunked".parse().unwrap());
        upstream.insert(header::CONTENT_LENGTH, "9".parse().unwrap());
        upstream.insert("x-config-revision", "7".parse().unwrap());
        let out = super::forwardable_headers(&upstream);
        assert!(out.get(header::CONNECTION).is_none());
        assert!(out.get(header::TRANSFER_ENCODING).is_none());
        assert!(out.get(header::CONTENT_LENGTH).is_none());
        assert_eq!(out.get("x-config-revision").unwrap(), "7");
    }
}
