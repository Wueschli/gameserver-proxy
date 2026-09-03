//! Internal API seam for **sniffer plugins** — game- / protocol-specific
//! first-bytes inspectors.
//!
//! A sniffer takes a read-only look at a connection's first bytes (TCP peek /
//! first UDP datagram) and, if it recognises the protocol, returns a
//! [`RouteHint`] (a hostname, an affinity key, a reject flag). The `sniffer`
//! route matcher then compares that hint.
//!
//! **There are no built-in sniffers.** Game-specific parsing is deliberately
//! *not* compiled into the proxy: it belongs in separately maintained plugins,
//! loaded at runtime. That loader (sandboxed — likely WASM — with a community
//! plugin repo) is a later roadmap phase; what lives here now is only the
//! contract it will implement, plus the wiring that feeds a hint into routing.
//! A `sniffer:` route therefore never matches today (it logs a warning at
//! listener start) unless a test provides one.
//!
//! Sniffers are read-only: they never see later bytes and never write. See
//! `docs/03`, `CLAUDE.md` "agnostic core".

use gsp_config::RouteHint;

/// A read-only first-bytes inspector. Implemented by loaded plugins (future);
/// the proxy binary ships none.
pub trait Sniffer: Send + Sync {
    fn name(&self) -> &'static str;
    /// Inspect the (bounded) first bytes. `None` = not recognised.
    fn sniff(&self, first: &[u8]) -> Option<RouteHint>;
}

/// Resolve a sniffer by its configured name. Returns `None` for every real
/// name until the plugin loader lands; a `#[cfg(test)]` build also knows
/// `"test-host"` (see the tests below) so the seam stays exercised.
pub fn sniffer(name: &str) -> Option<&'static dyn Sniffer> {
    match name {
        #[cfg(test)]
        "test-host" => Some(&tests::TestHost),
        _ => None,
    }
}

/// Log a warning if `listener` routes on a sniffer name that resolves to
/// nothing — those routes can never match until the plugin is loaded.
pub fn warn_if_missing(listener: &str, name: Option<&str>) {
    if let Some(name) = name {
        if sniffer(name).is_none() {
            tracing::warn!(
                %listener, sniffer = %name,
                "no such sniffer loaded — `sniffer` routes on this listener will never match \
                 (plugin loading is a later roadmap phase)"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal stand-in for a real plugin: `b"HOST:<name>\n..."` → that host.
    pub(super) struct TestHost;
    impl Sniffer for TestHost {
        fn name(&self) -> &'static str {
            "test-host"
        }
        fn sniff(&self, first: &[u8]) -> Option<RouteHint> {
            let rest = first.strip_prefix(b"HOST:")?;
            let end = rest.iter().position(|&b| b == b'\n')?;
            let host = std::str::from_utf8(&rest[..end]).ok()?;
            Some(RouteHint {
                host: Some(host.to_ascii_lowercase()),
                ..Default::default()
            })
        }
    }

    #[test]
    fn registry_has_no_builtins_but_knows_the_test_sniffer() {
        assert!(sniffer("minecraft").is_none());
        assert!(sniffer("sni").is_none());
        assert!(sniffer("nope").is_none());
        assert_eq!(sniffer("test-host").unwrap().name(), "test-host");
    }

    #[test]
    fn test_sniffer_extracts_the_host() {
        let hint = TestHost
            .sniff(b"HOST:Survival.Example.NET\npayload")
            .unwrap();
        assert_eq!(hint.host.as_deref(), Some("survival.example.net"));
        assert!(TestHost.sniff(b"no marker").is_none());
    }

    /// End to end: listener → sniffer → `MatchContext.sniff` → route. Lives here
    /// (not in `tests/`) so it can reach the `#[cfg(test)]` `test-host` sniffer.
    #[tokio::test]
    async fn sniffer_matcher_routes_a_connection_by_hint_host() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::{TcpListener, TcpStream};

        // Two backends, each writing an identifying byte on connect.
        async fn marker(tag: u8) -> std::net::SocketAddr {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = l.local_addr().unwrap();
            tokio::spawn(async move {
                while let Ok((mut s, _)) = l.accept().await {
                    tokio::spawn(async move {
                        let _ = s.write_all(&[tag]).await;
                        let mut buf = [0u8; 64];
                        while let Ok(n) = s.read(&mut buf).await {
                            if n == 0 {
                                break;
                            }
                        }
                    });
                }
            });
            addr
        }

        let survival = marker(b'S').await;
        let lobby = marker(b'L').await;
        let proxy = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap();

        let yaml = format!(
            r#"
pools:
  - name: survival
    targets: ["{survival}"]
  - name: lobby
    targets: ["{lobby}"]
listeners:
  - name: l
    bind: "{proxy}"
    routes:
      - match: {{ type: sniffer, sniffer: test-host, host: ["survival.example.net"] }}
        action: {{ pool: survival }}
      - match: {{ type: always }}
        action: {{ pool: lobby }}
"#
        );
        let cfg = gsp_config::parse_str(&yaml).unwrap();
        let runtime =
            crate::Runtime::start(crate::Snapshot::from_config(&cfg), Default::default(), 1);
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        let mark = |host: &'static str| async move {
            let mut c = TcpStream::connect(proxy).await.unwrap();
            c.write_all(format!("HOST:{host}\nrest").as_bytes())
                .await
                .unwrap();
            let mut m = [0u8; 1];
            c.read_exact(&mut m).await.unwrap();
            m[0]
        };

        assert_eq!(mark("survival.example.net").await, b'S');
        assert_eq!(mark("creative.example.net").await, b'L');

        runtime.shutdown().await;
    }
}
