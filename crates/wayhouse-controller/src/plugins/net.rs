//! A plugin's network: the transport its `http` capability sends through, and the source its
//! `${secret:NAME}` tokens are read from (design: the plugin system spec's "Network
//! access"; policy lives in `wayhouse_plugin_host::http`).
//!
//! The transport enforces the rules that need a socket: HTTPS only, no proxy (a proxy would
//! resolve the name itself and skip the address checks), no redirects (the engine follows
//! them hop by hop, re-checking each), and no destination in a private, loopback or
//! link-local range unless the host was approved for it. The name is resolved by a
//! resolver that applies the same rule on every connection, so a DNS answer that changes
//! between calls (rebinding) cannot reach a private address; the address it returns is the
//! one connected to. An IP literal is checked up front, since a literal skips DNS.

use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use tokio::runtime::Handle;
use wayhouse_plugin_host::http::{Hop, WireResponse, MAX_HEADERS, MAX_RESPONSE_BODY};
use wayhouse_plugin_host::{HttpEngine, SecretSource, SecretValue, Transport};

use super::secrets::KeyringHandle;
use super::{InstallRecord, PluginStore};

/// Whether `ip` is a public destination: not private, loopback, link-local, unspecified,
/// multicast, shared (carrier-grade NAT), unique-local or documentation space. An
/// IPv4-mapped IPv6 address is judged as the IPv4 address it wraps.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_multicast()
                || v4.is_broadcast()
                || v4.is_documentation()
                || (o[0] == 100 && (64..128).contains(&o[1])) // 100.64.0.0/10
                || (o[0] == 192 && o[1] == 0 && o[2] == 0) // 192.0.0.0/24
                || o[0] == 0
                || o[0] >= 240)
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public(IpAddr::V4(v4));
            }
            let s0 = v6.segments()[0];
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (s0 & 0xfe00) == 0xfc00 // fc00::/7 unique local
                || (s0 & 0xffc0) == 0xfe80 // fe80::/10 link local
                || (s0 == 0x2001 && v6.segments()[1] == 0x0db8)) // documentation
        }
    }
}

/// The addresses of `resolved` the connection may use: all of them for a host approved for
/// private ranges, otherwise the public ones.
pub fn permitted(resolved: Vec<SocketAddr>, private_ok: bool) -> Vec<SocketAddr> {
    resolved
        .into_iter()
        .filter(|a| private_ok || is_public(a.ip()))
        .collect()
}

/// Resolves names the way the OS does and drops what the host may not connect to.
struct GuardedResolver {
    /// Hosts approved for private destinations.
    private_ok: HashSet<String>,
}

impl Resolve for GuardedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_ascii_lowercase();
        let private_ok = self.private_ok.contains(&host);
        Box::pin(async move {
            let found: Vec<SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            let allowed = permitted(found, private_ok);
            if allowed.is_empty() {
                let why: Box<dyn std::error::Error + Send + Sync> =
                    format!("{host} resolves only to addresses this plugin may not reach").into();
                return Err(why);
            }
            Ok(Box::new(allowed.into_iter()) as Addrs)
        })
    }
}

/// Sends one hop on the controller's runtime.
pub struct ReqwestTransport {
    client: reqwest::Client,
    runtime: Handle,
}

impl ReqwestTransport {
    /// `private_ok`: the hosts the plugin's approval allows to resolve privately.
    pub fn new(private_ok: HashSet<String>, runtime: Handle) -> Result<Self, reqwest::Error> {
        let client = wayhouse_http::builder()
            // The fleet's protocol header is for fleet peers, not for a third party.
            .default_headers(reqwest::header::HeaderMap::new())
            .https_only(true)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .dns_resolver(Arc::new(GuardedResolver { private_ok }))
            .build()?;
        Ok(Self { client, runtime })
    }

    async fn send_async(&self, hop: &Hop) -> Result<WireResponse, String> {
        // A literal address never reaches the resolver.
        if let Some(url::Host::Ipv4(_) | url::Host::Ipv6(_)) = hop.url.host() {
            let ip: IpAddr = hop
                .url
                .host_str()
                .map(|h| h.trim_matches(['[', ']']))
                .and_then(|h| h.parse().ok())
                .ok_or("bad address")?;
            if !hop.allow_private && !is_public(ip) {
                return Err(format!(
                    "{ip} is a private address this plugin may not reach"
                ));
            }
        }
        let method =
            reqwest::Method::from_bytes(hop.method.as_bytes()).map_err(|e| e.to_string())?;
        let mut headers = reqwest::header::HeaderMap::new();
        for (name, value) in &hop.headers {
            let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| format!("bad header name {name:?}"))?;
            let mut value = reqwest::header::HeaderValue::from_str(value.as_str())
                .map_err(|_| "a header value is not valid".to_string())?;
            // Never printed by a debug formatter, whatever it carries.
            value.set_sensitive(true);
            headers.append(name, value);
        }
        let mut resp = self
            .client
            .request(method, hop.url.clone())
            .headers(headers)
            .body(hop.body.clone())
            .timeout(hop.timeout)
            .send()
            .await
            .map_err(|e| wayhouse_http::error_chain(&e))?;
        let status = resp.status().as_u16();
        let headers: Vec<(String, String)> = resp
            .headers()
            .iter()
            .take(MAX_HEADERS + 1)
            .map(|(n, v)| {
                (
                    n.as_str().to_string(),
                    String::from_utf8_lossy(v.as_bytes()).into_owned(),
                )
            })
            .collect();
        let mut body = Vec::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| wayhouse_http::error_chain(&e))?
        {
            if body.len() + chunk.len() > MAX_RESPONSE_BODY {
                return Err(format!("the response is over {MAX_RESPONSE_BODY} bytes"));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(WireResponse {
            status,
            headers,
            body,
        })
    }
}

impl Transport for ReqwestTransport {
    /// Called from a pool worker thread, outside any runtime.
    fn send(&self, hop: &Hop) -> Result<WireResponse, String> {
        self.runtime.block_on(self.send_async(hop))
    }
}

/// An install's secrets, read through the node's keyring.
pub struct InstallSecrets {
    pub id: String,
    pub store: PluginStore,
    pub keyring: KeyringHandle,
}

impl SecretSource for InstallSecrets {
    fn get(&self, slot: &str) -> Result<Option<SecretValue>, String> {
        let Some(stored) = self
            .store
            .get_secret(&self.id, slot)
            .map_err(|e| e.to_string())?
        else {
            return Ok(None);
        };
        self.keyring
            .current()
            .open(&self.id, slot, &stored.sealed)
            .map(Some)
            .map_err(|e| format!("held: {e}"))
    }
}

/// The engine for an install that declared `http`, or `None` for one that did not.
pub fn engine_for(
    rec: &InstallRecord,
    store: &PluginStore,
    keyring: &KeyringHandle,
    runtime: Handle,
) -> Result<Option<Arc<HttpEngine>>, String> {
    let Some(cap) = rec.approved.http.clone() else {
        return Ok(None);
    };
    let private_ok = cap
        .hosts
        .iter()
        .filter(|h| h.allow_private)
        .map(|h| h.host.clone())
        .collect();
    let transport = ReqwestTransport::new(private_ok, runtime).map_err(|e| e.to_string())?;
    let secrets = InstallSecrets {
        id: rec.id.clone(),
        store: store.clone(),
        keyring: keyring.clone(),
    };
    Ok(Some(Arc::new(HttpEngine::new(
        cap,
        rec.approved.secrets.clone(),
        Arc::new(secrets),
        Arc::new(transport),
    ))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use url::Url;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn only_public_addresses_are_public() {
        for public in [
            "8.8.8.8",
            "1.1.1.1",
            "93.184.216.34",
            "2606:4700:4700::1111",
            "::ffff:8.8.8.8",
        ] {
            assert!(is_public(ip(public)), "{public}");
        }
        for private in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254",
            "0.0.0.0",
            "100.64.0.1",
            "224.0.0.1",
            "255.255.255.255",
            "192.0.2.1",
            "192.0.0.8",
            "::1",
            "::",
            "fe80::1",
            "fc00::1",
            "fd12:3456::1",
            "ff02::1",
            "::ffff:10.0.0.1",
            "::ffff:127.0.0.1",
            "2001:db8::1",
        ] {
            assert!(!is_public(ip(private)), "{private}");
        }
    }

    #[test]
    fn a_resolution_keeps_only_what_the_approval_allows() {
        let sa = |s: &str| SocketAddr::new(ip(s), 0);
        let mixed = vec![sa("10.0.0.5"), sa("8.8.8.8"), sa("127.0.0.1")];
        assert_eq!(permitted(mixed.clone(), false), [sa("8.8.8.8")]);
        assert_eq!(permitted(mixed.clone(), true), mixed);
        assert!(permitted(vec![sa("10.0.0.5")], false).is_empty());
    }

    fn hop(url: &str, allow_private: bool) -> Hop {
        Hop {
            method: "GET".into(),
            url: Url::parse(url).unwrap(),
            headers: Vec::new(),
            body: Vec::new(),
            allow_private,
            timeout: Duration::from_secs(2),
        }
    }

    #[tokio::test]
    async fn an_address_literal_in_a_private_range_is_refused_without_connecting() {
        let t = ReqwestTransport::new(HashSet::new(), Handle::current()).unwrap();
        for url in [
            "https://127.0.0.1:1/",
            "https://10.0.0.5/",
            "https://[::1]:1/",
            "https://169.254.169.254/latest",
        ] {
            let e = t.send_async(&hop(url, false)).await.err().unwrap();
            assert!(e.contains("private address"), "{url}: {e}");
        }
    }

    #[tokio::test]
    async fn an_approved_private_literal_gets_as_far_as_connecting() {
        let t = ReqwestTransport::new(HashSet::new(), Handle::current()).unwrap();
        let e = t
            .send_async(&hop("https://127.0.0.1:1/", true))
            .await
            .err()
            .unwrap();
        assert!(!e.contains("private address"), "{e}");
    }

    #[tokio::test]
    async fn a_name_that_resolves_only_to_loopback_is_refused_unless_approved() {
        let strict = ReqwestTransport::new(HashSet::new(), Handle::current()).unwrap();
        let e = strict
            .send_async(&hop("https://localhost:1/", false))
            .await
            .err()
            .unwrap();
        assert!(e.contains("may not reach"), "{e}");
        let lax =
            ReqwestTransport::new(HashSet::from(["localhost".to_string()]), Handle::current())
                .unwrap();
        let e = lax
            .send_async(&hop("https://localhost:1/", true))
            .await
            .err()
            .unwrap();
        assert!(!e.contains("may not reach"), "{e}");
    }

    #[tokio::test]
    async fn plain_http_never_leaves_the_transport() {
        let t = ReqwestTransport::new(HashSet::new(), Handle::current()).unwrap();
        assert!(t.send_async(&hop("http://8.8.8.8/", false)).await.is_err());
    }
}
