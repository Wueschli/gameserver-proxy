//! Registers this origin with `gsp-controller`'s backend-peers registry
//! (`POST /peers`) — the wire shape is duplicated here rather than shared as a
//! library, the same precedent `controller_client`'s hand-parsed SSE and
//! `aggregator_client`'s duplicated `IngestPayload` already established.
//!
//! The controller is the tunnel address authority (spec
//! `docs/superpowers/specs/2026-10-02-tunnel-address-authority-design.md`):
//! the answer carries this origin's `tunnel_address`, which startup needs
//! *before* the WireGuard interface can be brought up.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

#[derive(Serialize)]
struct PeerRegistration<'a> {
    name: &'a str,
    pubkey: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    endpoint: Option<&'a str>,
    backends: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    tunnel_address: Option<&'a str>,
}

/// What the controller answers to a successful `POST /peers`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Registered {
    pub revision: u64,
    pub tunnel_address: String,
    #[serde(default)]
    pub tunnel_network: Option<String>,
}

/// This origin's identity/facts as submitted on every registration.
#[derive(Clone)]
pub struct Registration {
    pub name: String,
    pub pubkey: String,
    pub endpoint: Option<String>,
    /// `host:port` or the `:port` shorthand (the controller expands it).
    pub backends: Vec<String>,
    /// A pinned tunnel address (bare IP); `None` asks the controller to allocate.
    pub address: Option<String>,
}

fn body(reg: &Registration) -> PeerRegistration<'_> {
    PeerRegistration {
        name: &reg.name,
        pubkey: &reg.pubkey,
        endpoint: reg.endpoint.as_deref(),
        backends: &reg.backends,
        tunnel_address: reg.address.as_deref(),
    }
}

#[derive(Debug)]
pub enum RegisterError {
    /// The controller understood and refused (4xx other than 408/429):
    /// retrying cannot help.
    Rejected(String),
    /// Transport trouble or a 5xx: worth retrying.
    Transient(anyhow::Error),
}

impl std::fmt::Display for RegisterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegisterError::Rejected(m) => f.write_str(m),
            RegisterError::Transient(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for RegisterError {}

/// The HTTP client for every controller call. The timeouts are what make the
/// retry budget real: without them a controller that accepts TCP but never
/// answers would block startup (and the refresh loop) forever.
pub fn http_client() -> reqwest::Client {
    gsp_http::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .build()
        .expect("timeouts plus --ca-file roots (validated at startup) always build")
}

/// One registration attempt.
pub async fn register_once(
    client: &reqwest::Client,
    controller_url: &str,
    token: Option<&str>,
    reg: &Registration,
) -> Result<Registered, RegisterError> {
    let url = format!("{}/peers", controller_url.trim_end_matches('/'));
    let mut req = client.post(&url).json(&body(reg));
    if let Some(token) = token {
        req = req.bearer_auth(token);
    }
    let resp = req.send().await.map_err(|e| {
        RegisterError::Transient(
            anyhow::Error::new(e).context(format!("registering with the controller at {url}")),
        )
    })?;
    let status = resp.status();
    if status.is_success() {
        return resp.json().await.map_err(|e| {
            RegisterError::Transient(
                anyhow::Error::new(e).context("parsing the controller's registration response"),
            )
        });
    }
    let text = resp.text().await.unwrap_or_default();
    let msg = format!("controller rejected registration ({status}): {text}");
    // 408 and 429 are "not now", not "never": retry them like a 5xx.
    let retryable = status == reqwest::StatusCode::REQUEST_TIMEOUT
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS;
    if status.is_client_error() && !retryable {
        Err(RegisterError::Rejected(msg))
    } else {
        Err(RegisterError::Transient(anyhow::anyhow!(msg)))
    }
}

/// Retries transient failures with backoff until `budget` runs out; a
/// rejection (4xx other than 408/429) returns immediately.
pub async fn register_with_retry(
    client: &reqwest::Client,
    controller_url: &str,
    token: Option<&str>,
    reg: &Registration,
    budget: Duration,
) -> Result<Registered, RegisterError> {
    let deadline = Instant::now() + budget;
    let mut delay = Duration::from_millis(500);
    loop {
        match register_once(client, controller_url, token, reg).await {
            Ok(r) => return Ok(r),
            Err(e @ RegisterError::Rejected(_)) => return Err(e),
            Err(RegisterError::Transient(e)) => {
                if Instant::now() + delay >= deadline {
                    return Err(RegisterError::Transient(e));
                }
                tracing::warn!(error = %format!("{e:#}"), "controller not ready; retrying registration");
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(2));
            }
        }
    }
}

/// `Some(message)` when the controller now reports a different address than
/// the one this process is running with. Never fatal: a live interface must
/// not be torn down over a registry change — a restart applies the new one.
pub fn address_change(running: &str, reported: &str) -> Option<String> {
    (running != reported).then(|| {
        format!(
            "the controller now assigns tunnel address {reported} but this process is running \
             with {running}; keeping {running} — restart to apply the new address"
        )
    })
}

/// Re-registers every `interval` for as long as the process runs (a fixed
/// refresh: there is no "did anything change" signal to key off yet).
/// `running_address` is the bare IP the interface was brought up with.
pub async fn run(
    client: reqwest::Client,
    controller_url: String,
    token: Option<String>,
    reg: Registration,
    interval: Duration,
    running_address: String,
) {
    let mut warned = false;
    loop {
        match register_once(&client, &controller_url, token.as_deref(), &reg).await {
            Ok(r) => {
                tracing::info!(revision = r.revision, "registered with the controller");
                match address_change(&running_address, &r.tunnel_address) {
                    Some(msg) if !warned => {
                        tracing::error!("{msg}");
                        warned = true;
                    }
                    Some(_) => {}
                    None => warned = false,
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to register with the controller; will retry")
            }
        }
        tokio::time::sleep(interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg() -> Registration {
        Registration {
            name: "home".into(),
            pubkey: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
            endpoint: None,
            backends: vec![":25565".into()],
            address: None,
        }
    }

    #[test]
    fn optional_fields_are_omitted_from_the_body_when_absent() {
        let r = reg();
        let json = serde_json::to_string(&body(&r)).unwrap();
        assert!(!json.contains("endpoint"));
        assert!(!json.contains("tunnel_address"));
        assert!(json.contains("\"backends\":[\":25565\"]"));
    }

    #[test]
    fn a_pinned_address_and_an_endpoint_are_serialized() {
        let mut r = reg();
        r.endpoint = Some("203.0.113.7:51820".into());
        r.address = Some("10.60.0.2".into());
        let json = serde_json::to_string(&body(&r)).unwrap();
        assert!(json.contains("\"endpoint\":\"203.0.113.7:51820\""));
        assert!(json.contains("\"tunnel_address\":\"10.60.0.2\""));
    }

    #[test]
    fn address_change_reports_only_a_real_difference() {
        assert_eq!(address_change("10.60.0.5", "10.60.0.5"), None);
        let msg = address_change("10.60.0.5", "10.60.0.9").unwrap();
        assert!(msg.contains("10.60.0.5") && msg.contains("10.60.0.9"));
        assert!(msg.contains("restart"));
    }

    /// A one-shot HTTP server answering every request with a canned response.
    async fn canned(status_line: &'static str, body: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf).await;
                let resp = format!(
                    "HTTP/1.1 {status_line}\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes()).await;
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn a_successful_registration_parses_the_assigned_address_and_network() {
        let url = canned(
            "200 OK",
            r#"{"revision":4,"tunnel_address":"10.60.0.5","tunnel_network":"10.60.0.0/16"}"#,
        )
        .await;
        let got = register_once(&reqwest::Client::new(), &url, None, &reg())
            .await
            .unwrap();
        assert_eq!(got.tunnel_address, "10.60.0.5");
        assert_eq!(got.tunnel_network.as_deref(), Some("10.60.0.0/16"));
    }

    #[tokio::test]
    async fn a_4xx_is_a_permanent_rejection_carrying_the_controllers_message() {
        let url = canned(
            "409 Conflict",
            r#"{"error":"address 10.60.0.2 is already held by origin \"x\""}"#,
        )
        .await;
        match register_once(&reqwest::Client::new(), &url, None, &reg()).await {
            Err(RegisterError::Rejected(m)) => assert!(m.contains("already held"), "{m}"),
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_5xx_or_a_refused_connection_is_transient() {
        let url = canned("503 Service Unavailable", r#"{"error":"exhausted"}"#).await;
        assert!(matches!(
            register_once(&reqwest::Client::new(), &url, None, &reg()).await,
            Err(RegisterError::Transient(_))
        ));
        assert!(matches!(
            register_once(&reqwest::Client::new(), "http://127.0.0.1:1", None, &reg()).await,
            Err(RegisterError::Transient(_))
        ));
    }

    #[tokio::test]
    async fn a_408_or_429_is_transient_not_a_rejection() {
        for status in ["408 Request Timeout", "429 Too Many Requests"] {
            let url = canned(status, r#"{"error":"slow down"}"#).await;
            match register_once(&reqwest::Client::new(), &url, None, &reg()).await {
                Err(RegisterError::Transient(e)) => {
                    assert!(format!("{e:#}").contains("slow down"), "{e:#}")
                }
                other => panic!("{status}: expected Transient, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn retrying_gives_up_on_a_permanent_rejection_immediately() {
        let url = canned("409 Conflict", r#"{"error":"held"}"#).await;
        let started = std::time::Instant::now();
        let err = register_with_retry(
            &reqwest::Client::new(),
            &url,
            None,
            &reg(),
            Duration::from_secs(30),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, RegisterError::Rejected(_)));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "must not retry a 409"
        );
    }

    #[tokio::test]
    async fn a_hung_controller_does_not_block_startup_past_the_budget() {
        // Accepts the connection and never answers.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let mut held = Vec::new();
            loop {
                let (s, _) = listener.accept().await.unwrap();
                held.push(s);
            }
        });
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(200))
            .build()
            .unwrap();
        let started = std::time::Instant::now();
        let got = tokio::time::timeout(
            Duration::from_secs(10),
            register_with_retry(&client, &url, None, &reg(), Duration::from_secs(1)),
        )
        .await
        .expect("registration hung past the retry budget");
        assert!(matches!(got, Err(RegisterError::Transient(_))));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn the_production_client_has_a_request_timeout() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let mut held = Vec::new();
            loop {
                let (s, _) = listener.accept().await.unwrap();
                held.push(s);
            }
        });
        // Must give up on its own (10 s request timeout), never hang.
        let got = tokio::time::timeout(
            Duration::from_secs(20),
            register_once(&http_client(), &url, None, &reg()),
        )
        .await
        .expect("http_client() has no request timeout");
        assert!(matches!(got, Err(RegisterError::Transient(_))));
    }
}
