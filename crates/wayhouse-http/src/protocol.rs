//! The wire-protocol version every component-to-component HTTP route carries
//! (#185, `docs/superpowers/specs/2026-10-05-component-versioning-design.md`).
//!
//! Protocol numbers are independent of the product version. A *major* bump is an
//! incompatible wire change and makes peers refuse each other; a *minor* bump is
//! additive and accepted. Clients send [`HEADER`] on every request (the shared
//! [`crate::builder`] sets it as a default header); servers gate their
//! component-facing routers with [`gate`] (the `server` feature), which also
//! echoes the server's own version on every response.
//!
//! A request with no header is refused too (a caller that predates versioning)
//! except on [`gate_lenient`] routes, which operators also call by hand. An
//! unparsable or other-major value is always refused with `426`.

use std::fmt;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};

/// Header name carrying `<major>.<minor>`.
pub const HEADER: &str = "x-wayhouse-protocol";

/// Incompatible wire changes bump this (even while the product is 0.x).
pub const PROTOCOL_MAJOR: u16 = 1;
/// Additive wire changes bump this.
pub const PROTOCOL_MINOR: u16 = 0;

/// Counter, label `route_group` (`controller`, `aggregator`, `raft`): requests
/// refused because the caller's protocol major differs or the header is garbage.
pub const PROTOCOL_MISMATCH_TOTAL: &str = "wayhouse_protocol_mismatch_total";

static MISMATCHES: AtomicU64 = AtomicU64::new(0);

/// Requests this process has refused for an incompatible protocol, all route
/// groups together. A plain counter (not read back from the metrics recorder)
/// so `wayhouse` can report it to the aggregator, which flags a node red while
/// it keeps rising (#185).
pub fn mismatches_total() -> u64 {
    MISMATCHES.load(Ordering::Relaxed)
}

fn note_mismatch() {
    MISMATCHES.fetch_add(1, Ordering::Relaxed);
}

/// `major.minor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
}

impl ProtocolVersion {
    /// What this build speaks.
    pub const CURRENT: Self = Self {
        major: PROTOCOL_MAJOR,
        minor: PROTOCOL_MINOR,
    };

    /// Same major: the only thing a node refuses on.
    pub fn compatible(&self, other: &Self) -> bool {
        self.major == other.major
    }
}

impl fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// `s` is not `<major>.<minor>`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid protocol version {0:?}, expected <major>.<minor>")]
pub struct ParseVersionError(String);

impl FromStr for ProtocolVersion {
    type Err = ParseVersionError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bad = || ParseVersionError(s.to_owned());
        let (major, minor) = s.split_once('.').ok_or_else(bad)?;
        let num = |p: &str| {
            (!p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
                .then(|| p.parse::<u16>().ok())
                .flatten()
        };
        Ok(Self {
            major: num(major).ok_or_else(bad)?,
            minor: num(minor).ok_or_else(bad)?,
        })
    }
}

/// Request extension set by [`gate`]: the caller's version, `None` when it sent no
/// header. Handlers and SSE streams read it to gate new optional fields by the
/// receiver's version. Checked once on connect: an open stream is never torn
/// down by a later request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerProtocol(pub Option<ProtocolVersion>);

#[cfg(feature = "server")]
pub use server_side::{gate, gate_lenient};

#[cfg(feature = "server")]
mod server_side {
    use axum::extract::{Request, State};
    use axum::http::{header::HeaderValue, StatusCode};
    use axum::middleware::{from_fn_with_state, Next};
    use axum::response::{IntoResponse, Response};
    use axum::Router;

    use super::{note_mismatch, PeerProtocol, ProtocolVersion, HEADER, PROTOCOL_MISMATCH_TOTAL};

    #[derive(Clone, Copy)]
    struct Gate {
        name: &'static str,
        /// Refuse a request with no header too (a caller that predates
        /// versioning). Operator-facing routes stay lenient: `curl` has none.
        strict: bool,
    }

    /// Gate every route of `router` (a component-facing one: admin, UI,
    /// `/healthz` and `/metrics` stay ungated) on the caller's protocol major.
    /// A request with no header is refused too: components always send it, and
    /// a caller without it predates versioning. `route_group` labels
    /// [`PROTOCOL_MISMATCH_TOTAL`].
    pub fn gate<S: Clone + Send + Sync + 'static>(
        router: Router<S>,
        route_group: &'static str,
    ) -> Router<S> {
        with_gate(router, route_group, true)
    }

    /// [`gate`] for routes that operators also call by hand: a missing header
    /// is accepted, a present one must still be a compatible major.
    pub fn gate_lenient<S: Clone + Send + Sync + 'static>(
        router: Router<S>,
        route_group: &'static str,
    ) -> Router<S> {
        with_gate(router, route_group, false)
    }

    fn with_gate<S: Clone + Send + Sync + 'static>(
        router: Router<S>,
        name: &'static str,
        strict: bool,
    ) -> Router<S> {
        router.layer(from_fn_with_state(
            Gate { name, strict },
            require_compatible_protocol,
        ))
    }

    async fn require_compatible_protocol(
        State(group): State<Gate>,
        mut req: Request,
        next: Next,
    ) -> Response {
        let ours = ProtocolVersion::CURRENT;
        let mut peer = None;
        let raw = req.headers().get(HEADER);
        if raw.is_some() || group.strict {
            let shown = raw.map_or_else(
                || "(none: the caller predates protocol versioning)".to_owned(),
                |raw| String::from_utf8_lossy(raw.as_bytes()).into_owned(),
            );
            match shown.parse::<ProtocolVersion>() {
                Ok(v) if ours.compatible(&v) => peer = Some(v),
                bad => {
                    let group = group.name;
                    let peer_txt = bad.map_or(shown, |v| v.to_string());
                    tracing::warn!(
                        route_group = group, peer = %peer_txt, ours = %ours, path = %req.uri().path(),
                        "refusing a request with an incompatible wayhouse protocol: upgrade the older side"
                    );
                    metrics::counter!(PROTOCOL_MISMATCH_TOTAL, "route_group" => group).increment(1);
                    note_mismatch();
                    let body = format!(
                        "wayhouse protocol {peer_txt} is not compatible with this node ({ours}): upgrade the older side"
                    );
                    return with_version((StatusCode::UPGRADE_REQUIRED, body).into_response());
                }
            }
        }
        req.extensions_mut().insert(PeerProtocol(peer));
        with_version(next.run(req).await)
    }

    fn with_version(mut resp: Response) -> Response {
        let v = HeaderValue::from_str(&ProtocolVersion::CURRENT.to_string())
            .expect("digits and a dot are a valid header value");
        resp.headers_mut().insert(HEADER, v);
        resp
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_parses_and_displays() {
        let v: ProtocolVersion = "1.0".parse().unwrap();
        assert_eq!(v, ProtocolVersion { major: 1, minor: 0 });
        assert_eq!(v.to_string(), "1.0");
        assert_eq!("12.34".parse::<ProtocolVersion>().unwrap().minor, 34);
        for bad in [
            "0", "a.b", "1.2.3", "", ".", "1.", ".1", "+1.0", "-1.0", " 1.0", "1.0 ", "99999.0",
        ] {
            assert!(bad.parse::<ProtocolVersion>().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn compatible_means_same_major() {
        let a = ProtocolVersion { major: 1, minor: 0 };
        assert!(a.compatible(&ProtocolVersion { major: 1, minor: 7 }));
        assert!(!a.compatible(&ProtocolVersion { major: 2, minor: 0 }));
    }

    #[test]
    fn noting_a_mismatch_raises_the_count() {
        let before = mismatches_total();
        note_mismatch();
        // Other tests refuse requests concurrently, so only a lower bound holds.
        assert!(mismatches_total() > before);
    }

    #[test]
    fn current_matches_the_constants() {
        assert_eq!(
            ProtocolVersion::CURRENT.to_string(),
            format!("{PROTOCOL_MAJOR}.{PROTOCOL_MINOR}")
        );
    }
}

#[cfg(all(test, feature = "server"))]
mod gate_tests {
    use super::*;
    use axum::body::Body;
    use axum::extract::Extension;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use axum::Router;
    use tower::ServiceExt;

    fn routes() -> Router {
        Router::new().route(
            "/x",
            get(|Extension(p): Extension<PeerProtocol>| async move {
                p.0.map_or("none".to_owned(), |v| v.to_string())
            }),
        )
    }

    async fn call(header: Option<&str>) -> (StatusCode, Option<String>, String) {
        call_on(gate(routes(), "test"), header).await
    }

    async fn call_on(app: Router, header: Option<&str>) -> (StatusCode, Option<String>, String) {
        let mut req = Request::builder().uri("/x");
        if let Some(h) = header {
            req = req.header(HEADER, h);
        }
        let resp = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
        let status = resp.status();
        let ours = resp
            .headers()
            .get(HEADER)
            .map(|v| v.to_str().unwrap().to_owned());
        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        (status, ours, String::from_utf8(body.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn layer_rejects_missing_header_with_426() {
        let (s, ours, body) = call(None).await;
        assert_eq!(s, StatusCode::UPGRADE_REQUIRED);
        assert_eq!(ours.as_deref(), Some("1.0"));
        assert!(
            body.contains("predates protocol versioning") && body.contains("(1.0)"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn lenient_layer_accepts_missing_header_but_not_a_bad_one() {
        let app = || gate_lenient(routes(), "test");
        let (s, _, body) = call_on(app(), None).await;
        assert_eq!((s, body.as_str()), (StatusCode::OK, "none"));
        let (s, _, _) = call_on(app(), Some("2.0")).await;
        assert_eq!(s, StatusCode::UPGRADE_REQUIRED);
        let (s, _, _) = call_on(app(), Some("banana")).await;
        assert_eq!(s, StatusCode::UPGRADE_REQUIRED);
    }

    #[tokio::test]
    async fn layer_accepts_same_major_other_minor() {
        let (s, _, body) = call(Some("1.9")).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(body, "1.9");
    }

    #[tokio::test]
    async fn layer_rejects_other_major_with_426_and_body() {
        let (s, ours, body) = call(Some("2.0")).await;
        assert_eq!(s, StatusCode::UPGRADE_REQUIRED);
        assert_eq!(ours.as_deref(), Some("1.0"));
        assert_eq!(
            body,
            "wayhouse protocol 2.0 is not compatible with this node (1.0): upgrade the older side"
        );
    }

    #[tokio::test]
    async fn layer_rejects_garbage_value() {
        let (s, _, body) = call(Some("banana")).await;
        assert_eq!(s, StatusCode::UPGRADE_REQUIRED);
        assert!(body.contains("banana") && body.contains("(1.0)"), "{body}");
    }

    #[tokio::test]
    async fn layer_sets_response_header_on_success() {
        let (_, ours, _) = call(Some("1.0")).await;
        assert_eq!(ours.as_deref(), Some("1.0"));
    }

    #[tokio::test]
    async fn builder_sends_the_header() {
        let app = Router::new().route(
            "/h",
            get(|headers: axum::http::HeaderMap| async move {
                headers
                    .get(HEADER)
                    .map_or("none".to_owned(), |v| v.to_str().unwrap().to_owned())
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let body = crate::client()
            .get(format!("http://{addr}/h"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(body, "1.0");
    }
}
