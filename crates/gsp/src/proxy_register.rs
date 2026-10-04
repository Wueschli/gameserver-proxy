//! Registers this proxy with `gsp-controller`'s proxy-peers registry
//! (`POST /proxy-peers`) — the mirror image of `gsp-agent::register`,
//! duplicated rather than shared for the same reason: no library crate sits
//! between these two binaries.
//!
//! Exists so every origin's `gsp-agent` can learn about every edge proxy by
//! subscribing to that registry. The controller is also the tunnel address
//! authority (spec
//! `docs/superpowers/specs/2026-10-02-tunnel-address-authority-design.md`): the
//! answer carries this proxy's `tunnel_address`, which startup needs *before*
//! the WireGuard interface can be brought up.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use defguard_wireguard_rs::net::IpAddrMask;
use serde::{Deserialize, Serialize};

use crate::live_interface::LiveInterface;
use crate::tunnel_address;

#[derive(Serialize)]
struct ProxyRegistration<'a> {
    name: &'a str,
    pubkey: &'a str,
    endpoint: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    tunnel_address: Option<&'a str>,
    boot_id: &'a str,
}

/// What the controller answers to a successful `POST /proxy-peers`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Registered {
    pub revision: u64,
    pub tunnel_address: String,
    #[serde(default)]
    pub tunnel_network: Option<String>,
}

/// This proxy's identity/facts as submitted on every registration.
#[derive(Clone)]
pub struct Registration {
    pub name: String,
    pub pubkey: String,
    /// This proxy's public dial-out address (`ip:port`) — required for a proxy.
    pub endpoint: String,
    /// A pinned tunnel address (bare IP); `None` asks the controller to allocate.
    pub address: Option<String>,
    /// [`new_boot_id`], once per process: lets every origin's `gsp-agent`
    /// tell a restart from a routine re-registration.
    pub boot_id: String,
}

/// A fresh random id for this process start (128 bits, hex). A restarted
/// proxy has a new WireGuard interface but no endpoint for any origin, so it
/// cannot re-handshake by itself, and an agent whose kernel still holds the
/// old session would wait for the 120 s rekey. A changed boot id is what
/// makes the agent re-set the peer instead (`gsp-agent`'s `proxy_subscribe`).
pub fn new_boot_id() -> String {
    let bytes: [u8; 16] = rand::random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn body(reg: &Registration) -> ProxyRegistration<'_> {
    ProxyRegistration {
        name: &reg.name,
        pubkey: &reg.pubkey,
        endpoint: &reg.endpoint,
        tunnel_address: reg.address.as_deref(),
        boot_id: &reg.boot_id,
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
    client_with_timeouts(CONNECT_TIMEOUT, REQUEST_TIMEOUT)
}

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

fn client_with_timeouts(connect: Duration, request: Duration) -> reqwest::Client {
    gsp_http::builder()
        .connect_timeout(connect)
        .timeout(request)
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
    let url = format!("{}/proxy-peers", controller_url.trim_end_matches('/'));
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
    let msg = format!("controller rejected proxy registration ({status}): {text}");
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

/// What [`run`] needs to keep the live interface on the controller's address.
pub struct AddressSync {
    pub live: Arc<LiveInterface>,
    /// The operator's pinned `--address`/`--tunnel-address` ip/prefix, if any;
    /// supplies the prefix when the controller reports no network.
    pub pinned_cidr: Option<String>,
    /// Where the address is saved for a restart while the controller is down.
    pub path: PathBuf,
}

impl AddressSync {
    /// Applies the address the controller reported to the live interface:
    /// moves it when the address (or the network prefix) changed, and records
    /// the new address so a restart while the controller is down starts from
    /// it. `Ok(true)` when the interface moved.
    pub fn apply(&self, reported: &Registered) -> anyhow::Result<bool> {
        let cidr = tunnel_address::interface_cidr(
            &reported.tunnel_address,
            reported.tunnel_network.as_deref(),
            self.pinned_cidr.as_deref(),
        )?;
        let new: IpAddrMask = cidr
            .parse()
            .map_err(|e| anyhow::anyhow!("tunnel address {cidr:?} is not a valid ip/cidr: {e}"))?;
        if !self.live.needs_readdress(&new) {
            return Ok(false);
        }
        self.live.readdress(&new)?;
        tunnel_address::save(&self.path, &cidr)?;
        Ok(true)
    }
}

/// Re-registers every `interval` for as long as the process runs (a fixed
/// refresh: there is no "did anything change" signal to key off yet), and
/// moves the live interface whenever the controller's answer changes the
/// address — an operator released it and it was reallocated, say. The peers
/// follow on their own: each proxy re-reads the new address from the
/// registry. A failed move is retried on the next tick.
pub async fn run(
    client: reqwest::Client,
    controller_url: String,
    token: Option<String>,
    reg: Registration,
    interval: Duration,
    sync: AddressSync,
) {
    let sync = Arc::new(sync);
    loop {
        match register_once(&client, &controller_url, token.as_deref(), &reg).await {
            Ok(r) => {
                tracing::info!(
                    revision = r.revision,
                    "registered as a proxy peer with the controller"
                );
                // Moving the interface makes blocking netlink calls: keep it
                // off the async workers.
                let s = sync.clone();
                let moved = tokio::task::spawn_blocking(move || {
                    let from = s.live.address();
                    s.apply(&r).map(|moved| moved.then_some(from))
                })
                .await
                .unwrap_or_else(|e| Err(anyhow::anyhow!("address change task failed: {e}")));
                match moved {
                    Ok(Some(from)) => tracing::warn!(
                        from = %from, to = %sync.live.address(),
                        "the controller assigned a new tunnel address; moved the interface"
                    ),
                    Ok(None) => {}
                    Err(e) => {
                        tracing::error!(error = %format!("{e:#}"), "could not apply the controller's tunnel address; will retry")
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to register as a proxy peer; will retry")
            }
        }
        tokio::time::sleep(interval).await;
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn reg() -> Registration {
        Registration {
            name: "edge-1".into(),
            pubkey: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
            endpoint: "203.0.113.9:51820".into(),
            address: None,
            boot_id: "0123456789abcdef0123456789abcdef".into(),
        }
    }

    #[test]
    fn the_boot_id_is_sent_with_every_registration() {
        let json = serde_json::to_string(&body(&reg())).unwrap();
        assert!(
            json.contains("\"boot_id\":\"0123456789abcdef0123456789abcdef\""),
            "{json}"
        );
    }

    #[test]
    fn each_process_start_gets_a_fresh_boot_id() {
        let a = new_boot_id();
        let b = new_boot_id();
        assert_ne!(a, b);
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()), "{a}");
    }

    #[test]
    fn the_tunnel_address_is_omitted_from_the_body_when_not_pinned() {
        let json = serde_json::to_string(&body(&reg())).unwrap();
        assert!(!json.contains("tunnel_address"));
        assert!(json.contains("\"endpoint\":\"203.0.113.9:51820\""));
    }

    #[test]
    fn a_pinned_address_is_serialized() {
        let mut r = reg();
        r.address = Some("10.60.0.3".into());
        let json = serde_json::to_string(&body(&r)).unwrap();
        assert!(json.contains("\"tunnel_address\":\"10.60.0.3\""));
    }

    fn sync(live: &Arc<LiveInterface>, pinned: Option<&str>, path: &Path) -> AddressSync {
        AddressSync {
            live: live.clone(),
            pinned_cidr: pinned.map(str::to_string),
            path: path.to_path_buf(),
        }
    }

    fn reported(ip: &str) -> Registered {
        Registered {
            revision: 1,
            tunnel_address: ip.into(),
            tunnel_network: Some("10.60.0.0/24".into()),
        }
    }

    #[test]
    fn a_changed_address_moves_the_interface_and_is_saved() {
        use crate::live_interface::testing::{live, Log};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tunnel-address");
        let log = Log::default();
        let live = Arc::new(live(&[], &[], &log));
        assert!(sync(&live, None, &path)
            .apply(&reported("10.60.0.7"))
            .unwrap());
        assert_eq!(live.address().to_string(), "10.60.0.7/24");
        assert_eq!(tunnel_address::load(&path).as_deref(), Some("10.60.0.7/24"));
    }

    #[test]
    fn an_unchanged_address_touches_nothing() {
        use crate::live_interface::testing::{live, Log};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tunnel-address");
        let log = Log::default();
        let live = Arc::new(live(&[], &[], &log));
        assert!(!sync(&live, None, &path)
            .apply(&reported("10.60.0.2"))
            .unwrap());
        assert!(log.lock().unwrap().is_empty());
        assert!(!path.exists());
    }

    #[test]
    fn a_failed_move_saves_nothing() {
        use crate::live_interface::testing::{live, Log};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tunnel-address");
        let log = Log::default();
        let live = Arc::new(live(&["10.60.0.7/24"], &[], &log));
        assert!(sync(&live, None, &path)
            .apply(&reported("10.60.0.7"))
            .is_err());
        assert!(
            !path.exists(),
            "a restart must not start from an address that never came up"
        );
        assert_eq!(live.address().to_string(), "10.60.0.2/24");
    }

    #[test]
    fn pin_only_mode_keeps_the_pinned_prefix() {
        use crate::live_interface::testing::{live, Log};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tunnel-address");
        let log = Log::default();
        let live = Arc::new(live(&[], &[], &log));
        let r = Registered {
            revision: 1,
            tunnel_address: "10.60.0.9".into(),
            tunnel_network: None,
        };
        assert!(sync(&live, Some("10.60.0.9/24"), &path).apply(&r).unwrap());
        assert_eq!(live.address().to_string(), "10.60.0.9/24");
    }

    #[test]
    fn ipv6_spellings_of_one_address_are_not_a_change() {
        // `AddressSync::apply` decides by comparing parsed masks.
        let a: IpAddrMask = "fd49:0::5/64".parse().unwrap();
        let b: IpAddrMask = "FD49::5/64".parse().unwrap();
        let c: IpAddrMask = "fd49::6/64".parse().unwrap();
        assert_eq!(a, b);
        assert_ne!(a, c);
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
            r#"{"revision":4,"tunnel_address":"10.60.0.3","tunnel_network":"10.60.0.0/16"}"#,
        )
        .await;
        let got = register_once(&reqwest::Client::new(), &url, None, &reg())
            .await
            .unwrap();
        assert_eq!(got.tunnel_address, "10.60.0.3");
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
    async fn the_client_gives_up_on_a_controller_that_never_answers() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let mut held = Vec::new();
            loop {
                let (s, _) = listener.accept().await.unwrap();
                held.push(s);
            }
        });
        // Must give up on its own (the request timeout), never hang. The
        // production values are only checked to be finite; the hang itself is
        // exercised with a short timeout through the same builder.
        assert!(REQUEST_TIMEOUT <= Duration::from_secs(30));
        let client = client_with_timeouts(CONNECT_TIMEOUT, Duration::from_millis(300));
        let got = tokio::time::timeout(
            Duration::from_secs(5),
            register_once(&client, &url, None, &reg()),
        )
        .await
        .expect("the client has no request timeout");
        assert!(matches!(got, Err(RegisterError::Transient(_))));
    }
}
