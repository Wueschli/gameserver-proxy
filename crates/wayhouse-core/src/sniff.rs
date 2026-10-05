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
//! loaded at runtime by the Phase 9 loader (`wayhouse` binary, `wasmtime` — see
//! `docs/08` Phase 9). What lives here is the contract plugins implement (the
//! [`Sniffer`] trait), the [`Sniffers`] registry the loader populates, and the
//! wiring that feeds a hint into routing. A `sniffer:` route never matches on
//! an empty registry (it logs a warning at listener start).
//!
//! Sniffers are read-only: they never see later bytes and never write. See
//! `docs/03`, `AGENTS.md` "agnostic core".

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;

use wayhouse_config::RouteHint;

/// A read-only first-bytes inspector. Implemented by loaded plugins. `name` is
/// `&str`, not `&'static str`: a WASM plugin's name comes from its module file
/// name at load time, not a compiled-in constant.
pub trait Sniffer: Send + Sync {
    fn name(&self) -> &str;
    /// Inspect the (bounded) first bytes. `None` = not recognised.
    fn sniff(&self, first: &[u8]) -> Option<RouteHint>;
}

/// The live set of loaded sniffers, keyed by their configured name. Built once
/// at startup by the `wayhouse` binary's plugin loader; threaded `Runtime` →
/// `ListenerManager` → listener workers, the same seam as `Resolvers` /
/// `Option<Arc<GeoDb>>`. The default (and, until a loader configures one,
/// only) registry is empty.
///
/// Reads are lock-free (an [`ArcSwap`] over the name→plugin map, mirroring
/// [`crate::route_hint::RouteHints`]): every `Arc<Sniffers>` clone handed to a
/// listener worker points at the same instance, so [`Sniffers::replace`] (used
/// by the config-reload plugin rescan, phase 9 slice 4) is visible to every
/// worker immediately, with no replumbing needed through `Runtime` itself.
#[derive(Default)]
pub struct Sniffers {
    map: ArcSwap<HashMap<String, Arc<dyn Sniffer>>>,
}

impl Sniffers {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register one sniffer, replacing any earlier one under the same name.
    #[allow(clippy::needless_pass_by_value)] // registries take ownership of what they store
    pub fn register(&self, sniffer: Arc<dyn Sniffer>) {
        self.map.rcu(|cur| {
            let mut next = (**cur).clone();
            next.insert(sniffer.name().to_string(), sniffer.clone());
            next
        });
    }

    /// Atomically replace the whole registry with `map` (phase 9 slice 4: a
    /// config reload rescans the plugin dir and swaps in the new set — added
    /// modules appear, removed ones vanish, changed ones are already a fresh
    /// compile since the caller rebuilt `map` from scratch).
    pub fn replace(&self, map: HashMap<String, Arc<dyn Sniffer>>) {
        self.map.store(Arc::new(map));
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Sniffer>> {
        self.map.load().get(name).cloned()
    }

    /// Every currently-loaded sniffer's name, for a control-plane listing
    /// (`GET /admin/sniffers`) — never consulted on the hot path.
    pub fn names(&self) -> Vec<String> {
        self.map.load().keys().cloned().collect()
    }
}

/// Run a listener's sniffers (`names`, in config order) over `first`; the first
/// one that recognises the bytes wins, and its configured name is returned
/// with its hint. Later sniffers are not consulted, so a `reject` from an
/// earlier recogniser cannot be overridden by a later one. Names that resolve
/// to nothing in `sniffers` are skipped.
pub fn sniff_first<'a>(
    names: &'a [String],
    sniffers: &Sniffers,
    first: &[u8],
) -> Option<(&'a str, RouteHint)> {
    names.iter().find_map(|n| {
        let hint = sniffers.get(n)?.sniff(first)?;
        Some((n.as_str(), hint))
    })
}

/// Log a warning for each sniffer name `listener` routes on that resolves to
/// nothing in `sniffers` — those routes can never match until the plugin is
/// loaded.
pub fn warn_if_missing(listener: &str, names: &[String], sniffers: &Sniffers) {
    for name in names {
        if sniffers.get(name).is_none() {
            tracing::warn!(
                %listener, sniffer = %name,
                "no such sniffer loaded — `sniffer` routes on this listener will never match"
            );
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Minimal stand-in for a real plugin: `b"HOST:<name>\n..."` → that host.
    pub(crate) struct TestHost;
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

    /// Recognises `b"BAD"` first bytes and demands the connection be dropped.
    pub(crate) struct TestReject;
    impl Sniffer for TestReject {
        fn name(&self) -> &'static str {
            "test-reject"
        }
        fn sniff(&self, first: &[u8]) -> Option<RouteHint> {
            first.starts_with(b"BAD").then(|| RouteHint {
                reject: true,
                ..Default::default()
            })
        }
    }

    /// Recognises `b"TAG"` first bytes with a bare hint (no host).
    pub(crate) struct TestTag;
    impl Sniffer for TestTag {
        fn name(&self) -> &'static str {
            "test-tag"
        }
        fn sniff(&self, first: &[u8]) -> Option<RouteHint> {
            first.starts_with(b"TAG").then(RouteHint::default)
        }
    }

    /// A registry with the test sniffers (`test-host`, `test-reject`, `test-tag`).
    pub(crate) fn test_registry() -> Sniffers {
        let s = Sniffers::new();
        s.register(Arc::new(TestHost));
        s.register(Arc::new(TestReject));
        s.register(Arc::new(TestTag));
        s
    }

    fn names(n: &[&str]) -> Vec<String> {
        n.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn sniff_first_returns_the_first_recogniser_in_config_order() {
        let reg = test_registry();
        let both = names(&["test-tag", "test-host"]);
        // only the second recognises
        let (n, h) = sniff_first(
            &both,
            &reg,
            b"HOST:a.example
",
        )
        .unwrap();
        assert_eq!((n, h.host.as_deref()), ("test-host", Some("a.example")));
        // only the first recognises
        assert_eq!(sniff_first(&both, &reg, b"TAG!").unwrap().0, "test-tag");
        // nobody recognises
        assert!(sniff_first(&both, &reg, b"zzz").is_none());
        // an unloaded name is skipped, not fatal
        let with_missing = names(&["nope", "test-tag"]);
        assert_eq!(
            sniff_first(&with_missing, &reg, b"TAG").unwrap().0,
            "test-tag"
        );
        // earlier recogniser wins; its reject is final
        let reject_first = names(&["test-reject", "test-tag"]);
        assert!(sniff_first(&reject_first, &reg, b"BAD").unwrap().1.reject);
    }

    #[test]
    fn registry_has_no_builtins_but_knows_the_test_sniffer() {
        let s = test_registry();
        assert!(s.get("minecraft").is_none());
        assert!(s.get("sni").is_none());
        assert!(s.get("nope").is_none());
        assert_eq!(s.get("test-host").unwrap().name(), "test-host");
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
    /// (not in `tests/`) so it can reach the `test-host` sniffer.
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
        let cfg = wayhouse_config::parse_str(&yaml).unwrap();
        let runtime = crate::Runtime::start_with_sniffers(
            crate::Snapshot::from_config(&cfg),
            Arc::default(),
            None,
            Arc::new(test_registry()),
            1,
        );
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

        runtime
            .shutdown_with_grace(std::time::Duration::from_millis(100))
            .await;
    }

    /// Two sniffers on one listener: each connection is routed by whichever
    /// sniffer recognises its first bytes.
    #[tokio::test]
    async fn two_sniffers_on_one_listener_route_by_recognising_sniffer() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::{TcpListener, TcpStream};

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

        let host_be = marker(b'H').await;
        let tag_be = marker(b'T').await;
        let fallback = marker(b'F').await;
        let proxy = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap();
        let yaml = format!(
            r#"
pools:
  - name: h
    targets: ["{host_be}"]
  - name: t
    targets: ["{tag_be}"]
  - name: f
    targets: ["{fallback}"]
listeners:
  - name: l
    bind: "{proxy}"
    routes:
      - match: {{ type: sniffer, sniffer: test-host }}
        action: {{ pool: h }}
      - match: {{ type: sniffer, sniffer: test-tag }}
        action: {{ pool: t }}
      - match: {{ type: always }}
        action: {{ pool: f }}
"#
        );
        let cfg = wayhouse_config::parse_str(&yaml).unwrap();
        assert_eq!(cfg.listeners[0].sniffers, ["test-host", "test-tag"]);
        let runtime = crate::Runtime::start_with_sniffers(
            crate::Snapshot::from_config(&cfg),
            Arc::default(),
            None,
            Arc::new(test_registry()),
            1,
        );
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        let mark = |payload: &'static [u8]| async move {
            let mut c = TcpStream::connect(proxy).await.unwrap();
            c.write_all(payload).await.unwrap();
            let mut m = [0u8; 1];
            c.read_exact(&mut m).await.unwrap();
            m[0]
        };
        assert_eq!(mark(b"HOST:a.example\nrest").await, b'H');
        assert_eq!(mark(b"TAG and more").await, b'T');
        assert_eq!(mark(b"unrecognised").await, b'F');

        runtime
            .shutdown_with_grace(std::time::Duration::from_millis(100))
            .await;
    }

    /// A `reject` hint drops the TCP connection even though an `always` route
    /// would otherwise catch it — and a benign connection still routes.
    #[tokio::test]
    async fn sniffer_reject_drops_the_tcp_connection() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::{TcpListener, TcpStream};

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

        let lobby = marker(b'L').await;
        let proxy = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap();

        let yaml = format!(
            r#"
pools:
  - name: lobby
    targets: ["{lobby}"]
listeners:
  - name: l
    bind: "{proxy}"
    routes:
      - match: {{ type: sniffer, sniffer: test-reject }}
        action: {{ pool: lobby }}
      - match: {{ type: always }}
        action: {{ pool: lobby }}
"#
        );
        let cfg = wayhouse_config::parse_str(&yaml).unwrap();
        let runtime = crate::Runtime::start_with_sniffers(
            crate::Snapshot::from_config(&cfg),
            Arc::default(),
            None,
            Arc::new(test_registry()),
            1,
        );
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        // Rejected: the proxy closes the connection without a backend byte.
        let mut bad = TcpStream::connect(proxy).await.unwrap();
        bad.write_all(b"BADpayload").await.unwrap();
        let mut m = [0u8; 1];
        let r = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            bad.read_exact(&mut m),
        )
        .await
        .expect("proxy should close a rejected connection, not hang");
        assert!(r.is_err(), "rejected connection should get EOF, not a byte");

        // Not rejected: still routes through `always`.
        let mut ok = TcpStream::connect(proxy).await.unwrap();
        ok.write_all(b"good payload").await.unwrap();
        let mut m2 = [0u8; 1];
        ok.read_exact(&mut m2).await.unwrap();
        assert_eq!(m2[0], b'L');

        runtime
            .shutdown_with_grace(std::time::Duration::from_millis(100))
            .await;
    }

    /// A `reject` hint on the first datagram opens no UDP session and sends no
    /// reply (amplifier-safe), even with an `always` route present.
    #[tokio::test]
    async fn sniffer_reject_drops_the_udp_datagram_with_no_reply() {
        use tokio::net::{TcpListener, UdpSocket};

        // Backend that echoes every datagram back — proves the proxy never
        // forwarded (and thus never relayed a reply).
        let backend = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let backend_addr = backend.local_addr().unwrap();
        tokio::spawn(async move {
            let mut b = [0u8; 1024];
            while let Ok((n, from)) = backend.recv_from(&mut b).await {
                let _ = backend.send_to(&b[..n], from).await;
            }
        });

        let proxy = {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            l.local_addr().unwrap()
        };
        let yaml = format!(
            r#"
pools:
  - name: p
    targets: ["{backend_addr}"]
    health_check: {{ type: none }}
listeners:
  - name: u
    bind: "{proxy}"
    protocol: udp
    routes:
      - match: {{ type: sniffer, sniffer: test-reject }}
        action: {{ pool: p }}
      - match: {{ type: always }}
        action: {{ pool: p }}
"#
        );
        let cfg = wayhouse_config::parse_str(&yaml).unwrap();
        let runtime = crate::Runtime::start_with_sniffers(
            crate::Snapshot::from_config(&cfg),
            Arc::default(),
            None,
            Arc::new(test_registry()),
            1,
        );
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client.connect(proxy).await.unwrap();
        client.send(b"BAD flood").await.unwrap();

        let mut buf = [0u8; 64];
        let got =
            tokio::time::timeout(std::time::Duration::from_millis(400), client.recv(&mut buf))
                .await;
        assert!(got.is_err(), "a rejected datagram must draw no reply");

        runtime
            .shutdown_with_grace(std::time::Duration::from_millis(100))
            .await;
    }
}
