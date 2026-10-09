//! The webhook and event triggers' data: what the host hands a guest and what it accepts back.
//!
//! The controller authenticates the caller and enforces size and rate limits before any of
//! this exists (`docs/plugins.md` "Webhooks"). Here the host bounds what the *guest* returns:
//! a status, a few allowlisted headers and a capped body.

use std::collections::BTreeMap;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde::{Deserialize, Serialize};

/// Largest request body a webhook may carry (the listener enforces it before reading).
pub const MAX_WEBHOOK_BODY: usize = 256 * 1024;
/// Largest response body a guest may return.
pub const MAX_RESPONSE_BODY: usize = 64 * 1024;
/// Most response headers a guest may set.
pub const MAX_RESPONSE_HEADERS: usize = 16;
/// Largest `webhook_respond` document (base64 inflates the body a third).
pub const MAX_RESPONSE_DOC: usize = MAX_RESPONSE_BODY * 2 + 4096;
/// Largest event payload handed to `on_event`.
pub const MAX_EVENT_PAYLOAD: usize = 16 * 1024;

/// The events a plugin may subscribe to. The list only grows; payloads only gain optional
/// fields (docs/plugins.md "Events").
pub const EVENT_KINDS: &[&str] = &["config_revision", "plugin_changed"];

/// What the guest sees of a webhook call. The authorization header never appears.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebhookRequest {
    pub method: String,
    /// The path after `/plugins/<id>/hook` (empty or starting with `/`).
    pub suffix: String,
    pub query: String,
    /// Lowercase names.
    pub headers: BTreeMap<String, String>,
    #[serde(serialize_with = "ser_b64", deserialize_with = "de_b64")]
    pub body: Vec<u8>,
    /// Stable across retries of one request; forward it to systems that accept one.
    pub idempotency_key: String,
}

fn ser_b64<S: serde::Serializer>(b: &[u8], s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&STANDARD.encode(b))
}

fn de_b64<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
    let s = String::deserialize(d)?;
    STANDARD.decode(s).map_err(serde::de::Error::custom)
}

/// What the guest answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

impl Default for WebhookResponse {
    /// A guest that answers nothing gets `204`.
    fn default() -> Self {
        Self {
            status: 204,
            headers: BTreeMap::new(),
            body: Vec::new(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GuestResponse {
    status: u16,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    body: Option<String>,
}

/// Response headers a guest may set: a content type and custom `x-` headers. Never
/// `Set-Cookie`, hop-by-hop or framing headers.
pub fn header_allowed(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    (n == "content-type" || (n.starts_with("x-") && n.len() > 2))
        && n.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Parses and checks a `webhook_respond` document.
pub fn parse_response(raw: &[u8]) -> Result<WebhookResponse, String> {
    let g: GuestResponse =
        serde_json::from_slice(raw).map_err(|e| format!("not a response: {e}"))?;
    if !(200..=599).contains(&g.status) {
        return Err(format!("status {} is not 200 to 599", g.status));
    }
    if g.headers.len() > MAX_RESPONSE_HEADERS {
        return Err(format!("more than {MAX_RESPONSE_HEADERS} headers"));
    }
    let mut headers = BTreeMap::new();
    for (k, v) in g.headers {
        if !header_allowed(&k) {
            return Err(format!(
                "header {k:?} is not allowed (content-type and x-* only)"
            ));
        }
        if v.len() > 1024 || v.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0) {
            return Err(format!("header {k:?} has a bad value"));
        }
        headers.insert(k.to_ascii_lowercase(), v);
    }
    let body = match g.body {
        Some(b) => STANDARD
            .decode(b)
            .map_err(|_| "body is not base64".to_string())?,
        None => Vec::new(),
    };
    if body.len() > MAX_RESPONSE_BODY {
        return Err(format!("body is over {MAX_RESPONSE_BODY} bytes"));
    }
    Ok(WebhookResponse {
        status: g.status,
        headers,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_response_parses_with_lowercase_headers() {
        let r = parse_response(
            br#"{"status":202,"headers":{"Content-Type":"text/plain","X-Run":"7"},"body":"b2s="}"#,
        )
        .unwrap();
        assert_eq!((r.status, r.body.as_slice()), (202, &b"ok"[..]));
        assert_eq!(r.headers["content-type"], "text/plain");
    }

    #[test]
    fn bad_responses_are_refused() {
        for bad in [
            r#"{"status":99}"#,
            r#"{"status":600}"#,
            r#"{"status":200,"headers":{"Set-Cookie":"a=b"}}"#,
            r#"{"status":200,"headers":{"Content-Length":"1"}}"#,
            r#"{"status":200,"headers":{"X-A":"a\r\nX-B: b"}}"#,
            r#"{"status":200,"body":"!!"}"#,
            r#"{"status":200,"extra":1}"#,
            "nope",
        ] {
            assert!(parse_response(bad.as_bytes()).is_err(), "{bad}");
        }
        let big = STANDARD.encode(vec![0u8; MAX_RESPONSE_BODY + 1]);
        assert!(parse_response(format!(r#"{{"status":200,"body":"{big}"}}"#).as_bytes()).is_err());
        let many: BTreeMap<String, String> = (0..=MAX_RESPONSE_HEADERS)
            .map(|i| (format!("x-h{i}"), "v".to_string()))
            .collect();
        let doc = serde_json::json!({"status":200,"headers":many}).to_string();
        assert!(parse_response(doc.as_bytes()).is_err());
    }
}
