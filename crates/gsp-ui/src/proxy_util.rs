//! Shared helper for `crate::aggregator_proxy` and `crate::controller_proxy`
//! — both are thin pass-through proxies to a machine service, and both need
//! to forward the upstream response's headers faithfully, not just its
//! status and body.

/// The upstream response's headers, minus the hop-by-hop ones that don't
/// make sense to forward verbatim (`axum` recomputes `content-length` for
/// the body we actually send; `connection`/`transfer-encoding` describe
/// *this* hop's framing, not the content). Everything else — notably
/// `gsp-controller`'s `X-Config-Revision` — passes through, so a caller
/// reading a response header from `gsp-ui` sees exactly what the upstream
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
