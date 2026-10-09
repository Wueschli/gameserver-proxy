//! The `http` capability: policy for a plugin's outbound HTTPS calls (design: the plugin
//! system spec's "Network access" and the secret storage design's "Binding and expansion").
//!
//! The [`HttpEngine`] decides everything that does not need a socket, so it is testable
//! without one: only approved `host:port` pairs over HTTPS, `${secret:NAME}` expanded by the
//! host in header values only and only for hosts the slot is bound to, every redirect hop
//! re-checked against the same rules, budgets, and a [`Scrubber`] over everything the guest
//! gets back. The [`Transport`] it drives sends exactly one hop and enforces the
//! connect-time rules (no private address unless the host was approved for it, the resolved
//! address pinned for the connection); the controller supplies it.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use url::{Host, Url};
use zeroize::Zeroizing;

use crate::caps::{valid_slot_name, HttpCap, SecretSlot};
use crate::secret::{Scrubber, SecretSource};

/// Guest requests one call may make (redirect hops are not counted).
pub const MAX_HTTP_CALLS: usize = 8;
/// Redirects followed for one request.
pub const MAX_REDIRECTS: usize = 3;
pub const MAX_REQUEST_BODY: usize = 256 * 1024;
pub const MAX_RESPONSE_BODY: usize = 1024 * 1024;
pub const MAX_HEADERS: usize = 64;
/// One hop's time limit.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// All the HTTP time one guest call may spend.
pub const CALL_BUDGET: Duration = Duration::from_secs(20);

/// Request headers a guest may not set: the host owns framing and routing.
const FORBIDDEN_HEADERS: &[&str] = &[
    "host",
    "content-length",
    "transfer-encoding",
    "connection",
    "upgrade",
    "te",
    "trailer",
    "keep-alive",
    "proxy-authorization",
    "proxy-connection",
];

/// A header value that may carry an expanded secret: zeroed when dropped.
pub type HeaderValue = Zeroizing<String>;

/// One request on the wire, after policy and secret expansion.
pub struct Hop {
    pub method: String,
    pub url: Url,
    pub headers: Vec<(String, HeaderValue)>,
    pub body: Vec<u8>,
    /// The host was approved for private, loopback and link-local addresses.
    pub allow_private: bool,
    pub timeout: Duration,
}

pub struct WireResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Sends one hop without following redirects. Implementations refuse a destination address
/// the hop's `allow_private` does not permit (checked on every connection, so a changing DNS
/// answer cannot move a call somewhere else) and cap the response at [`MAX_RESPONSE_BODY`].
pub trait Transport: Send + Sync {
    fn send(&self, hop: &Hop) -> Result<WireResponse, String>;
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GuestRequest {
    method: String,
    url: String,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    /// Base64.
    #[serde(default)]
    body: Option<String>,
}

#[derive(Serialize)]
struct GuestResponse {
    status: u16,
    headers: BTreeMap<String, String>,
    /// Base64.
    body: String,
}

/// What one guest call has used so far.
pub struct HttpCall {
    started: Instant,
    calls: usize,
    /// Everything expanded during the call: scrubbed from what the guest sees.
    pub scrubber: Scrubber,
}

impl Default for HttpCall {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            calls: 0,
            scrubber: Scrubber::default(),
        }
    }
}

pub struct HttpEngine {
    cap: HttpCap,
    slots: Vec<SecretSlot>,
    secrets: Arc<dyn SecretSource>,
    transport: Arc<dyn Transport>,
}

fn host_string(url: &Url) -> Option<String> {
    Some(match url.host()? {
        Host::Domain(d) => d.to_ascii_lowercase(),
        Host::Ipv4(ip) => ip.to_string(),
        Host::Ipv6(ip) => ip.to_string(),
    })
}

/// The canonical text of a declared host (an IP literal is normalised).
fn canonical(host: &str) -> String {
    host.parse::<std::net::IpAddr>()
        .map_or_else(|_| host.to_string(), |ip| ip.to_string())
}

impl HttpEngine {
    /// `cap` and `slots` are the plugin's capabilities as approved.
    pub fn new(
        cap: HttpCap,
        slots: Vec<SecretSlot>,
        secrets: Arc<dyn SecretSource>,
        transport: Arc<dyn Transport>,
    ) -> Self {
        Self {
            cap,
            slots,
            secrets,
            transport,
        }
    }

    /// Runs one guest request document and returns the response document: the response,
    /// or `{"error": "..."}`. Never panics on guest input; an error carries no secret.
    pub fn call(&self, state: &mut HttpCall, raw: &[u8]) -> Vec<u8> {
        let doc = match self.run(state, raw) {
            Ok(r) => serde_json::to_vec(&r),
            Err(why) => {
                let why = state.scrubber.scrub_str(&why);
                serde_json::to_vec(&serde_json::json!({ "error": why }))
            }
        };
        doc.unwrap_or_else(|_| br#"{"error":"could not encode the response"}"#.to_vec())
    }

    fn run(&self, state: &mut HttpCall, raw: &[u8]) -> Result<GuestResponse, String> {
        if state.calls >= MAX_HTTP_CALLS {
            return Err(format!(
                "http budget exhausted: {MAX_HTTP_CALLS} calls per run"
            ));
        }
        state.calls += 1;
        let req: GuestRequest =
            serde_json::from_slice(raw).map_err(|e| format!("malformed request: {e}"))?;
        let mut method = req.method.to_ascii_uppercase();
        if !matches!(
            method.as_str(),
            "GET" | "HEAD" | "POST" | "PUT" | "PATCH" | "DELETE"
        ) {
            return Err(format!("method {method} is not allowed"));
        }
        let mut url = Url::parse(&req.url).map_err(|e| format!("bad url: {e}"))?;
        let mut body = match &req.body {
            Some(b64) => STANDARD
                .decode(b64)
                .map_err(|_| "the body is not valid base64".to_string())?,
            None => Vec::new(),
        };
        if body.len() > MAX_REQUEST_BODY {
            return Err(format!("the request body is over {MAX_REQUEST_BODY} bytes"));
        }
        let templates = validate_headers(&req.headers)?;
        for hop_no in 0..=MAX_REDIRECTS {
            let (host, allow_private) = self.check_target(&url)?;
            let headers = self.expand(&templates, &host, state)?;
            let left = CALL_BUDGET.saturating_sub(state.started.elapsed());
            if left.is_zero() {
                return Err("http budget exhausted: the call used its time".into());
            }
            let hop = Hop {
                method: method.clone(),
                url: url.clone(),
                headers,
                body: body.clone(),
                allow_private,
                timeout: left.min(REQUEST_TIMEOUT),
            };
            let resp = self.transport.send(&hop)?;
            drop(hop);
            let location = resp
                .headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case("location"))
                .map(|(_, v)| v.clone());
            if let (true, Some(location)) =
                (matches!(resp.status, 301 | 302 | 303 | 307 | 308), location)
            {
                if hop_no == MAX_REDIRECTS {
                    return Err(format!("more than {MAX_REDIRECTS} redirects"));
                }
                url = url
                    .join(&location)
                    .map_err(|e| format!("bad redirect target: {e}"))?;
                let drops_body = resp.status == 303 && method != "HEAD"
                    || matches!(resp.status, 301 | 302) && method == "POST";
                if drops_body {
                    method = "GET".into();
                    body = Vec::new();
                }
                continue;
            }
            return self.respond(state, &resp);
        }
        Err(format!("more than {MAX_REDIRECTS} redirects"))
    }

    /// The host and its private-address approval, or why the target is refused.
    fn check_target(&self, url: &Url) -> Result<(String, bool), String> {
        if url.scheme() != "https" {
            return Err("only https is allowed".into());
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err("credentials in the url are not allowed".into());
        }
        let host = host_string(url).ok_or("the url has no host")?;
        let port = url.port_or_known_default().unwrap_or(443);
        let approved = self
            .cap
            .hosts
            .iter()
            .find(|h| canonical(&h.host) == host && h.port == port)
            .ok_or_else(|| format!("{host}:{port} is not an approved host"))?;
        Ok((host, approved.allow_private))
    }

    /// The headers of one hop with `${secret:NAME}` expanded for `host`.
    fn expand(
        &self,
        templates: &[(String, String)],
        host: &str,
        state: &mut HttpCall,
    ) -> Result<Vec<(String, HeaderValue)>, String> {
        const OPEN: &str = "${secret:";
        let mut out = Vec::with_capacity(templates.len());
        for (name, template) in templates {
            let mut value = Zeroizing::new(String::new());
            let mut rest = template.as_str();
            while let Some(at) = rest.find(OPEN) {
                value.push_str(&rest[..at]);
                let after = &rest[at + OPEN.len()..];
                let end = after
                    .find('}')
                    .ok_or("an unterminated ${secret:...} token")?;
                let slot_name = &after[..end];
                if !valid_slot_name(slot_name) {
                    return Err(format!("{slot_name:?} is not a secret slot name"));
                }
                let slot = self
                    .slots
                    .iter()
                    .find(|s| s.name == slot_name)
                    .ok_or_else(|| format!("secret slot {slot_name} is not approved"))?;
                if !slot.hosts.iter().any(|h| canonical(h) == host) {
                    return Err(format!("secret slot {slot_name} is not bound to {host}"));
                }
                let secret = self
                    .secrets
                    .get(slot_name)?
                    .ok_or_else(|| format!("secret slot {slot_name} is not set"))?;
                let text = std::str::from_utf8(secret.expose())
                    .ok()
                    .filter(|t| !t.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0))
                    .ok_or_else(|| {
                        format!("secret slot {slot_name} is not a valid header value")
                    })?;
                value.push_str(text);
                state.scrubber.add(&secret);
                rest = &after[end + 1..];
            }
            value.push_str(rest);
            out.push((name.clone(), value));
        }
        Ok(out)
    }

    fn respond(&self, state: &HttpCall, resp: &WireResponse) -> Result<GuestResponse, String> {
        if resp.body.len() > MAX_RESPONSE_BODY || resp.headers.len() > MAX_HEADERS {
            return Err("the response is over the size limits".into());
        }
        let headers = resp
            .headers
            .iter()
            .map(|(n, v)| (n.to_ascii_lowercase(), state.scrubber.scrub_str(v)))
            .collect();
        Ok(GuestResponse {
            status: resp.status,
            headers,
            body: STANDARD.encode(state.scrubber.scrub_bytes(&resp.body)),
        })
    }
}

fn validate_headers(headers: &BTreeMap<String, String>) -> Result<Vec<(String, String)>, String> {
    if headers.len() > MAX_HEADERS {
        return Err(format!("more than {MAX_HEADERS} request headers"));
    }
    let mut out = Vec::new();
    for (name, value) in headers {
        let token = !name.is_empty()
            && name.len() <= 128
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
        if !token || FORBIDDEN_HEADERS.contains(&name.to_ascii_lowercase().as_str()) {
            return Err(format!("header {name:?} is not allowed"));
        }
        if value.len() > 8192 || value.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0) {
            return Err(format!("header {name} has an invalid value"));
        }
        out.push((name.clone(), value.clone()));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::HttpHost;
    use crate::secret::SecretValue;
    use std::collections::HashMap;
    use std::sync::Mutex;

    type Seen = Vec<(String, String, Vec<(String, String)>, Vec<u8>, bool)>;

    /// Records each hop (method, url, headers, body, allow_private) and answers from `script`
    /// by url, echoing the Authorization header back when the script says `echo`.
    #[derive(Default)]
    struct Fake {
        seen: Mutex<Seen>,
        script: Mutex<HashMap<String, WireResponse>>,
        echo: bool,
    }

    impl Fake {
        fn answer(&self, url: &str, status: u16, headers: &[(&str, &str)], body: &[u8]) {
            self.script.lock().unwrap().insert(
                url.into(),
                WireResponse {
                    status,
                    headers: headers
                        .iter()
                        .map(|(a, b)| ((*a).into(), (*b).into()))
                        .collect(),
                    body: body.to_vec(),
                },
            );
        }
        fn hops(&self) -> Seen {
            std::mem::take(&mut *self.seen.lock().unwrap())
        }
    }

    impl Transport for Fake {
        fn send(&self, hop: &Hop) -> Result<WireResponse, String> {
            let headers: Vec<(String, String)> = hop
                .headers
                .iter()
                .map(|(n, v)| (n.clone(), v.to_string()))
                .collect();
            self.seen.lock().unwrap().push((
                hop.method.clone(),
                hop.url.to_string(),
                headers.clone(),
                hop.body.clone(),
                hop.allow_private,
            ));
            if self.echo {
                let auth = headers
                    .iter()
                    .find(|(n, _)| n == "Authorization")
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default();
                return Ok(WireResponse {
                    status: 200,
                    headers: vec![("X-Echo".into(), auth.clone())],
                    body: auth.into_bytes(),
                });
            }
            self.script
                .lock()
                .unwrap()
                .remove(hop.url.as_str())
                .ok_or_else(|| format!("no scripted answer for {}", hop.url))
        }
    }

    struct Secrets(HashMap<&'static str, &'static str>);

    impl SecretSource for Secrets {
        fn get(&self, slot: &str) -> Result<Option<SecretValue>, String> {
            Ok(self
                .0
                .get(slot)
                .map(|v| SecretValue::new(v.as_bytes().to_vec())))
        }
    }

    const TOKEN: &str = "s3cr3t-token-value";

    fn engine(transport: &Arc<Fake>) -> HttpEngine {
        let hosts = [
            ("panel.example", 443, false),
            ("other.example", 443, false),
            ("10.0.0.5", 8443, true),
        ];
        HttpEngine::new(
            HttpCap {
                hosts: hosts
                    .iter()
                    .map(|(h, p, a)| HttpHost {
                        host: (*h).into(),
                        port: *p,
                        allow_private: *a,
                    })
                    .collect(),
            },
            vec![SecretSlot {
                name: "PANEL_TOKEN".into(),
                description: String::new(),
                hosts: vec!["panel.example".into(), "10.0.0.5".into()],
            }],
            Arc::new(Secrets(HashMap::from([("PANEL_TOKEN", TOKEN)]))),
            transport.clone(),
        )
    }

    #[allow(clippy::needless_pass_by_value)]
    fn ask(e: &HttpEngine, req: serde_json::Value) -> serde_json::Value {
        serde_json::from_slice(&e.call(&mut HttpCall::default(), req.to_string().as_bytes()))
            .unwrap()
    }

    fn get(url: &str) -> serde_json::Value {
        serde_json::json!({"method": "GET", "url": url})
    }

    #[test]
    fn an_approved_host_is_called_and_the_response_comes_back() {
        let t = Arc::new(Fake::default());
        t.answer(
            "https://panel.example/api",
            200,
            &[("Content-Type", "text/plain")],
            b"hello",
        );
        let r = ask(&engine(&t), get("https://panel.example/api"));
        assert_eq!(r["status"], 200);
        assert_eq!(r["headers"]["content-type"], "text/plain");
        assert_eq!(
            STANDARD.decode(r["body"].as_str().unwrap()).unwrap(),
            b"hello"
        );
        let hops = t.hops();
        assert_eq!((hops[0].0.as_str(), hops[0].4), ("GET", false));
    }

    #[test]
    fn anything_not_approved_is_refused_before_the_transport_sees_it() {
        let t = Arc::new(Fake::default());
        let e = engine(&t);
        for url in [
            "https://evil.example/",
            "http://panel.example/",
            "https://panel.example:8443/",
            "https://user:pw@panel.example/",
            "https://panel.example.evil.example/",
            "ftp://panel.example/",
            "not a url",
        ] {
            let r = ask(&e, get(url));
            assert!(r["error"].is_string(), "{url}: {r}");
        }
        assert!(t.hops().is_empty());
    }

    #[test]
    fn a_private_host_is_flagged_for_the_transport_and_a_public_one_is_not() {
        let t = Arc::new(Fake::default());
        t.answer("https://10.0.0.5:8443/x", 204, &[], b"");
        t.answer("https://panel.example/x", 204, &[], b"");
        let e = engine(&t);
        ask(&e, get("https://10.0.0.5:8443/x"));
        ask(&e, get("https://panel.example/x"));
        let flags: Vec<bool> = t.hops().iter().map(|h| h.4).collect();
        assert_eq!(flags, [true, false]);
    }

    #[test]
    fn a_secret_is_expanded_in_header_values_only_for_a_bound_host() {
        let t = Arc::new(Fake::default());
        t.answer(
            "https://panel.example/q?t=${secret:PANEL_TOKEN}",
            200,
            &[],
            b"ok",
        );
        let e = engine(&t);
        let mut req = get("https://panel.example/q?t=${secret:PANEL_TOKEN}");
        req["headers"] = serde_json::json!({"Authorization": "Bearer ${secret:PANEL_TOKEN}"});
        req["body"] = serde_json::json!(STANDARD.encode("${secret:PANEL_TOKEN}"));
        req["method"] = "POST".into();
        let r = ask(&e, req);
        assert_eq!(r["status"], 200, "{r}");
        let hops = t.hops();
        let auth = &hops[0]
            .2
            .iter()
            .find(|(n, _)| n == "Authorization")
            .unwrap()
            .1;
        assert_eq!(auth, &format!("Bearer {TOKEN}"));
        assert!(
            hops[0].1.contains("%7Bsecret:") || hops[0].1.contains("${secret:"),
            "the query is literal: {}",
            hops[0].1
        );
        assert!(!hops[0].1.contains(TOKEN));
        assert_eq!(hops[0].3, b"${secret:PANEL_TOKEN}", "the body is literal");
    }

    #[test]
    fn a_secret_that_cannot_be_expanded_fails_the_call_and_is_never_sent_literally() {
        let t = Arc::new(Fake::default());
        let e = engine(&t);
        for token in [
            "${secret:UNKNOWN}",     // not an approved slot
            "${secret:panel_token}", // not a slot name
            "${secret:PANEL_TOKEN",  // unterminated
        ] {
            let mut req = get("https://panel.example/");
            req["headers"] = serde_json::json!({"Authorization": token});
            assert!(ask(&e, req)["error"].is_string(), "{token}");
        }
        // Bound to panel.example but not to other.example.
        let mut req = get("https://other.example/");
        req["headers"] = serde_json::json!({"Authorization": "${secret:PANEL_TOKEN}"});
        let r = ask(&e, req);
        assert!(r["error"].as_str().unwrap().contains("not bound"), "{r}");
        assert!(t.hops().is_empty());
    }

    #[test]
    fn an_unset_secret_fails_the_call() {
        let t = Arc::new(Fake::default());
        let e = HttpEngine::new(
            HttpCap {
                hosts: vec![HttpHost {
                    host: "panel.example".into(),
                    port: 443,
                    allow_private: false,
                }],
            },
            vec![SecretSlot {
                name: "PANEL_TOKEN".into(),
                description: String::new(),
                hosts: vec!["panel.example".into()],
            }],
            Arc::new(Secrets(HashMap::new())),
            t.clone(),
        );
        let mut req = get("https://panel.example/");
        req["headers"] = serde_json::json!({"Authorization": "${secret:PANEL_TOKEN}"});
        assert!(ask(&e, req)["error"].as_str().unwrap().contains("not set"));
        assert!(t.hops().is_empty());
    }

    #[test]
    fn a_destination_that_echoes_the_secret_gives_the_guest_redacted_text() {
        let t = Arc::new(Fake {
            echo: true,
            ..Fake::default()
        });
        let e = engine(&t);
        let mut req = get("https://panel.example/");
        req["headers"] = serde_json::json!({"Authorization": "Bearer ${secret:PANEL_TOKEN}"});
        let doc = serde_json::to_string(&ask(&e, req)).unwrap();
        assert!(!doc.contains(TOKEN), "{doc}");
        let r: serde_json::Value = serde_json::from_str(&doc).unwrap();
        assert_eq!(r["headers"]["x-echo"], "Bearer [redacted]");
        assert_eq!(
            STANDARD.decode(r["body"].as_str().unwrap()).unwrap(),
            b"Bearer [redacted]"
        );
    }

    #[test]
    fn a_redirect_to_an_approved_host_that_is_not_bound_fails_and_never_receives_the_secret() {
        let t = Arc::new(Fake::default());
        t.answer(
            "https://panel.example/a",
            302,
            &[("Location", "https://other.example/b")],
            b"",
        );
        t.answer("https://other.example/b", 200, &[], b"leaked");
        let e = engine(&t);
        let mut req = get("https://panel.example/a");
        req["headers"] = serde_json::json!({"Authorization": "Bearer ${secret:PANEL_TOKEN}"});
        let r = ask(&e, req);
        assert!(r["error"].as_str().unwrap().contains("not bound"), "{r}");
        let hops = t.hops();
        assert_eq!(hops.len(), 1, "the second hop was never sent");
    }

    #[test]
    fn redirects_stay_inside_the_approved_hosts_and_are_bounded() {
        let t = Arc::new(Fake::default());
        t.answer("https://panel.example/a", 301, &[("Location", "/b")], b"");
        t.answer(
            "https://panel.example/b",
            307,
            &[("Location", "https://evil.example/")],
            b"",
        );
        let r = ask(&engine(&t), get("https://panel.example/a"));
        assert!(
            r["error"]
                .as_str()
                .unwrap()
                .contains("not an approved host"),
            "{r}"
        );

        let t = Arc::new(Fake::default());
        for (i, next) in ["b", "c", "d", "e"].iter().enumerate() {
            let from = ["a", "b", "c", "d"][i];
            t.answer(
                &format!("https://panel.example/{from}"),
                302,
                &[("Location", next)],
                b"",
            );
        }
        let r = ask(&engine(&t), get("https://panel.example/a"));
        assert!(r["error"].as_str().unwrap().contains("redirects"), "{r}");
    }

    #[test]
    fn a_redirect_to_a_bound_host_expands_the_secret_again_and_a_303_becomes_a_get() {
        let t = Arc::new(Fake::default());
        t.answer(
            "https://panel.example/a",
            303,
            &[("Location", "https://10.0.0.5:8443/b")],
            b"",
        );
        t.answer("https://10.0.0.5:8443/b", 200, &[], b"done");
        let e = engine(&t);
        let mut req = get("https://panel.example/a");
        req["method"] = "POST".into();
        req["body"] = serde_json::json!(STANDARD.encode("payload"));
        req["headers"] = serde_json::json!({"Authorization": "Bearer ${secret:PANEL_TOKEN}"});
        assert_eq!(ask(&e, req)["status"], 200);
        let hops = t.hops();
        assert_eq!(hops.len(), 2);
        assert_eq!(
            (hops[1].0.as_str(), hops[1].3.len(), hops[1].4),
            ("GET", 0, true)
        );
        assert!(hops[1]
            .2
            .iter()
            .any(|(_, v)| v == &format!("Bearer {TOKEN}")));
    }

    #[test]
    fn the_host_owns_framing_headers_and_rejects_bad_ones() {
        let t = Arc::new(Fake::default());
        let e = engine(&t);
        for headers in [
            serde_json::json!({"Host": "evil.example"}),
            serde_json::json!({"content-length": "1"}),
            serde_json::json!({"X-A": "a\r\nX-B: b"}),
            serde_json::json!({"bad name": "a"}),
        ] {
            let mut req = get("https://panel.example/");
            req["headers"] = headers;
            assert!(ask(&e, req)["error"].is_string());
        }
        assert!(t.hops().is_empty());
    }

    #[test]
    fn a_call_gets_a_bounded_number_of_requests() {
        let t = Arc::new(Fake::default());
        let e = engine(&t);
        let mut call = HttpCall::default();
        for i in 0..=MAX_HTTP_CALLS {
            t.answer("https://panel.example/", 200, &[], b"");
            let doc: serde_json::Value = serde_json::from_slice(&e.call(
                &mut call,
                get("https://panel.example/").to_string().as_bytes(),
            ))
            .unwrap();
            assert_eq!(
                doc["error"].is_string(),
                i == MAX_HTTP_CALLS,
                "request {i}: {doc}"
            );
        }
    }
}
