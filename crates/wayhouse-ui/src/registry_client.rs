//! Fetching registry indexes and sniffer artifacts, with the guards a client
//! that follows operator-supplied URLs needs: https only, no private
//! destinations (also not through a redirect or a hostname that resolves to
//! one), a byte cap enforced while streaming, and short timeouts.
//!
//! The guards live here and in the DNS resolver below, not at call sites, so
//! every hop of every request is covered.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use wayhouse_registry::{parse_index, Index, IndexError, MAX_INDEX_BYTES};

/// Largest signature file fetched.
pub const MAX_SIGNATURE_BYTES: u64 = 4 * 1024;
const MAX_REDIRECTS: usize = 5;
const INDEX_TTL: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("only https:// URLs are fetched")]
    Scheme,
    #[error("{0} is not a public address")]
    PrivateAddress(IpAddr),
    #[error("the registry did not answer in time")]
    Timeout,
    #[error("the response is larger than the allowed size")]
    TooLarge,
    #[error("the registry answered with status {0}")]
    Status(u16),
    #[error("invalid index: {0}")]
    Index(#[from] IndexError),
    #[error("{0}")]
    Io(String),
}

/// True for an address a registry may legitimately live at: anything that is
/// not loopback, private, link-local, unspecified, multicast, broadcast, CGNAT
/// or unique-local. An IPv4-mapped IPv6 address is judged as its IPv4 form.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_public_v4(v4),
            None => is_public_v6(v6),
        },
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    let cgnat = o[0] == 100 && (o[1] & 0xc0) == 64;
    !(ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        || cgnat)
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    let unique_local = (s[0] & 0xfe00) == 0xfc00;
    let link_local = (s[0] & 0xffc0) == 0xfe80;
    !(ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() || unique_local || link_local)
}

/// Why a URL (the first one, or a redirect target) may not be fetched. `None`
/// when it is fine. A hostname is judged later, by the resolver.
fn url_refusal(url: &reqwest::Url, allow_http: bool, allow_private: bool) -> Option<FetchError> {
    if url.scheme() != "https" && !(allow_http && url.scheme() == "http") {
        return Some(FetchError::Scheme);
    }
    if !allow_private {
        let literal = url
            .host_str()
            .and_then(|h| h.trim_matches(['[', ']']).parse::<IpAddr>().ok());
        if let Some(ip) = literal.filter(|ip| !is_public(*ip)) {
            return Some(FetchError::PrivateAddress(ip));
        }
    }
    None
}

/// Resolves names but drops every non-public answer, so a name pointing at
/// 127.0.0.1 or 169.254.169.254 never connects, whatever hop it is.
struct PublicOnlyResolver;

/// The public addresses `host` resolves to, or why there are none.
async fn resolve_public(host: &str) -> Result<Vec<SocketAddr>, String> {
    let all: Vec<SocketAddr> = tokio::net::lookup_host((host, 0))
        .await
        .map_err(|e| e.to_string())?
        .collect();
    let public: Vec<SocketAddr> = all.iter().copied().filter(|a| is_public(a.ip())).collect();
    if public.is_empty() {
        let shown = all.first().map_or(host.to_string(), |a| a.ip().to_string());
        return Err(format!("{host} resolves to non-public address {shown}"));
    }
    Ok(public)
}

impl Resolve for PublicOnlyResolver {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(async move {
            let public = resolve_public(name.as_str()).await?;
            Ok(Box::new(public.into_iter()) as Addrs)
        })
    }
}

type IndexCache = Arc<Mutex<HashMap<String, (Instant, Arc<Index>)>>>;

#[derive(Clone)]
pub struct RegistryClient {
    http: reqwest::Client,
    allow_http: bool,
    allow_private: bool,
    ttl: Duration,
    cache: IndexCache,
    /// Tests only: `https://registry.test` URLs (the only kind an index may hold) are
    /// fetched from this local server instead.
    #[cfg(test)]
    rewrite_to: Option<String>,
}

impl RegistryClient {
    /// The production client: https only, public destinations only.
    pub fn new() -> Self {
        Self::build(false, false, Duration::from_secs(30), INDEX_TTL)
    }

    /// For tests against a local plain-http server: both guards off. Not
    /// available to production code.
    #[cfg(test)]
    pub(crate) fn for_tests() -> Self {
        Self::build(true, true, Duration::from_secs(2), INDEX_TTL)
    }

    fn build(allow_http: bool, allow_private: bool, total: Duration, ttl: Duration) -> Self {
        let policy = reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= MAX_REDIRECTS {
                return attempt.error("too many redirects");
            }
            match url_refusal(attempt.url(), allow_http, allow_private) {
                Some(why) => attempt.error(why.to_string()),
                None => attempt.follow(),
            }
        });
        let mut builder = wayhouse_http::builder()
            .redirect(policy)
            .connect_timeout(Duration::from_secs(10))
            .timeout(total);
        if !allow_private {
            builder = builder.dns_resolver(Arc::new(PublicOnlyResolver));
        }
        Self {
            http: builder.build().expect("static client configuration"),
            allow_http,
            allow_private,
            ttl,
            cache: Arc::default(),
            #[cfg(test)]
            rewrite_to: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_rewrite(mut self, local_base: &str) -> Self {
        self.rewrite_to = Some(local_base.trim_end_matches('/').to_string());
        self
    }

    pub fn invalidate(&self, url: &str) {
        self.cache.lock().unwrap().remove(url);
    }

    pub async fn fetch_index(&self, url: &str) -> Result<Arc<Index>, FetchError> {
        if let Some((at, index)) = self.cache.lock().unwrap().get(url) {
            if at.elapsed() < self.ttl {
                return Ok(index.clone());
            }
        }
        let bytes = self.get(url, MAX_INDEX_BYTES as u64).await?;
        let index = Arc::new(parse_index(&bytes)?);
        self.cache
            .lock()
            .unwrap()
            .insert(url.to_string(), (Instant::now(), index.clone()));
        Ok(index)
    }

    pub async fn fetch_artifact(&self, url: &str, max_bytes: u64) -> Result<Vec<u8>, FetchError> {
        self.get(url, max_bytes).await
    }

    pub async fn fetch_signature(&self, url: &str) -> Result<Vec<u8>, FetchError> {
        self.get(url, MAX_SIGNATURE_BYTES).await
    }

    /// GET with the byte cap enforced while the body streams in.
    async fn get(&self, url: &str, max_bytes: u64) -> Result<Vec<u8>, FetchError> {
        #[cfg(test)]
        let url = &match &self.rewrite_to {
            Some(local) => url.replacen("https://registry.test", local, 1),
            None => url.to_string(),
        };
        let parsed = reqwest::Url::parse(url).map_err(|e| FetchError::Io(e.to_string()))?;
        if let Some(why) = url_refusal(&parsed, self.allow_http, self.allow_private) {
            return Err(why);
        }
        let mut resp = self
            .http
            .get(parsed)
            .send()
            .await
            .map_err(|e| map_err(&e))?;
        if !resp.status().is_success() {
            return Err(FetchError::Status(resp.status().as_u16()));
        }
        if resp.content_length().is_some_and(|n| n > max_bytes) {
            return Err(FetchError::TooLarge);
        }
        let mut body = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|e| map_err(&e))? {
            if body.len() as u64 + chunk.len() as u64 > max_bytes {
                return Err(FetchError::TooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }
}

impl Default for RegistryClient {
    fn default() -> Self {
        Self::new()
    }
}

fn map_err(e: &reqwest::Error) -> FetchError {
    if e.is_timeout() {
        FetchError::Timeout
    } else {
        FetchError::Io(wayhouse_http::error_chain(e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use axum::Router;
    use std::sync::atomic::{AtomicUsize, Ordering};

    async fn serve(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    const INDEX: &str = include_str!("../../wayhouse-registry/tests/fixtures/example-index.json");

    fn index_server(hits: Arc<AtomicUsize>) -> Router {
        Router::new().route(
            "/index.json",
            get(move || {
                let hits = hits.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    INDEX
                }
            }),
        )
    }

    #[tokio::test]
    async fn fetches_and_parses_index() {
        let base = serve(index_server(Arc::default())).await;
        let index = RegistryClient::for_tests()
            .fetch_index(&format!("{base}/index.json"))
            .await
            .unwrap();
        assert!(!index.sniffers.is_empty());
    }

    #[tokio::test]
    async fn index_is_cached_for_the_ttl_and_refetched_after_invalidate() {
        let hits = Arc::new(AtomicUsize::new(0));
        let base = serve(index_server(hits.clone())).await;
        let url = format!("{base}/index.json");
        let c = RegistryClient::for_tests();
        c.fetch_index(&url).await.unwrap();
        c.fetch_index(&url).await.unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        c.invalidate(&url);
        c.fetch_index(&url).await.unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn an_expired_cache_entry_is_refetched() {
        let hits = Arc::new(AtomicUsize::new(0));
        let base = serve(index_server(hits.clone())).await;
        let url = format!("{base}/index.json");
        let c = RegistryClient::build(true, true, Duration::from_secs(2), Duration::ZERO);
        c.fetch_index(&url).await.unwrap();
        c.fetch_index(&url).await.unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn oversize_index_is_cut_off() {
        let big = "x".repeat(MAX_INDEX_BYTES + 1);
        let app = Router::new().route("/i", get(move || async move { big }));
        let base = serve(app).await;
        let r = RegistryClient::for_tests()
            .fetch_index(&format!("{base}/i"))
            .await;
        assert!(matches!(r, Err(FetchError::TooLarge)), "{r:?}");
    }

    #[tokio::test]
    async fn artifact_larger_than_max_bytes_is_cut_off_while_streaming() {
        // No Content-Length: a chunked body that never ends.
        let app = Router::new().route(
            "/a",
            get(|| async {
                let s = futures_util::stream::repeat_with(|| {
                    Ok::<_, std::io::Error>(bytes::Bytes::from(vec![0u8; 8192]))
                });
                axum::body::Body::from_stream(s)
            }),
        );
        let base = serve(app).await;
        let r = RegistryClient::for_tests()
            .fetch_artifact(&format!("{base}/a"), 100_000)
            .await;
        assert!(matches!(r, Err(FetchError::TooLarge)), "{r:?}");
    }

    #[tokio::test]
    async fn non_200_is_a_status_error() {
        let app = Router::new();
        let base = serve(app).await;
        let r = RegistryClient::for_tests()
            .fetch_artifact(&format!("{base}/missing"), 10)
            .await;
        assert!(matches!(r, Err(FetchError::Status(404))), "{r:?}");
    }

    #[tokio::test]
    async fn slow_server_times_out() {
        let app = Router::new().route(
            "/slow",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(10)).await;
                "late"
            }),
        );
        let base = serve(app).await;
        let r = RegistryClient::build(true, true, Duration::from_millis(300), INDEX_TTL)
            .fetch_artifact(&format!("{base}/slow"), 10)
            .await;
        assert!(matches!(r, Err(FetchError::Timeout)), "{r:?}");
    }

    #[tokio::test]
    async fn a_redirect_loop_stops_after_five_hops() {
        let app = Router::new().route(
            "/loop",
            get(|| async { (axum::http::StatusCode::FOUND, [("location", "/loop")]) }),
        );
        let base = serve(app).await;
        let r = RegistryClient::for_tests()
            .fetch_artifact(&format!("{base}/loop"), 10)
            .await;
        assert!(r.is_err(), "{r:?}");
    }

    #[tokio::test]
    async fn production_client_refuses_http_and_private_literals_before_connecting() {
        let c = RegistryClient::new();
        for (u, want_private) in [
            ("http://example.com/x", false),
            ("https://127.0.0.1/x", true),
            ("https://10.0.0.1/x", true),
            ("https://169.254.169.254/latest", true),
            ("https://[::1]/x", true),
            ("https://[fc00::1]/x", true),
            ("https://0.0.0.0/x", true),
            ("https://[::ffff:127.0.0.1]/x", true),
        ] {
            let r = c.fetch_artifact(u, 10).await;
            match r {
                Err(FetchError::PrivateAddress(_)) if want_private => {}
                Err(FetchError::Scheme) if !want_private => {}
                other => panic!("{u}: {other:?}"),
            }
        }
    }

    #[test]
    fn redirect_targets_get_the_same_checks() {
        let to = |s: &str| reqwest::Url::parse(s).unwrap();
        assert!(matches!(
            url_refusal(&to("http://example.com/x"), false, false),
            Some(FetchError::Scheme)
        ));
        assert!(matches!(
            url_refusal(&to("https://127.0.0.1/x"), false, false),
            Some(FetchError::PrivateAddress(_))
        ));
        assert!(url_refusal(&to("https://example.com/x"), false, false).is_none());
    }

    #[test]
    fn address_classification() {
        for private in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "0.0.0.0",
            "100.64.0.1",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "::",
            "fc00::1",
            "fd12::1",
            "fe80::1",
            "ff02::1",
            "::ffff:10.0.0.1",
        ] {
            assert!(!is_public(private.parse().unwrap()), "{private}");
        }
        for public in ["1.1.1.1", "8.8.8.8", "140.82.112.3", "2606:4700::1111"] {
            assert!(is_public(public.parse().unwrap()), "{public}");
        }
    }

    #[tokio::test]
    async fn the_resolver_refuses_names_that_resolve_to_loopback() {
        let r = resolve_public("localhost").await;
        assert!(r.unwrap_err().contains("non-public"));
    }
}
