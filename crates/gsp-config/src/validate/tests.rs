use super::*;
use crate::*;

/// Build a `MatchContext` from `"src" , "local"` strings and optional bytes.
fn ctx<'a>(src: &str, local: &str, first_bytes: &'a [u8]) -> MatchContext<'a> {
    MatchContext {
        src: src.parse().unwrap(),
        local: local.parse().unwrap(),
        first_bytes,
        sniff: None,
    }
}

/// Like [`ctx`] but with a sniffer hint (from sniffer `minecraft`) attached.
fn ctx_sniff<'a>(hint: &'a RouteHint) -> MatchContext<'a> {
    ctx_sniff_by("minecraft", hint)
}

fn ctx_sniff_by<'a>(name: &'a str, hint: &'a RouteHint) -> MatchContext<'a> {
    MatchContext {
        src: "9.9.9.9:1".parse().unwrap(),
        local: "1.1.1.1:25565".parse().unwrap(),
        first_bytes: &[],
        sniff: Some((name, hint)),
    }
}

const MINIMAL: &str = r#"
pools:
  - name: local
    targets: ["127.0.0.1:9001"]
listeners:
  - name: public-tcp
    bind: "0.0.0.0:7777"
    pool: local
"#;

#[test]
fn parses_minimal_config() {
    let cfg = parse_str(MINIMAL).expect("should parse");
    assert_eq!(cfg.listeners.len(), 1);
    assert_eq!(cfg.pools.len(), 1);
    assert_eq!(cfg.admin_listen.port(), 9900);
    assert_eq!(cfg.admin_auth_token, None);
    assert_eq!(cfg.pools[0].connect_timeout.as_millis(), 300);
    assert_eq!(cfg.pools[0].balancer, Balancer::RoundRobin);
    assert_eq!(cfg.pools[0].health_check.rise, 2);
    assert_eq!(cfg.pools[0].health_check.fall, 3);
    assert!(cfg.pools[0].max_sessions.is_none());
    assert_eq!(cfg.pools[0].proxy_protocol, ProxyProtocol::None);
}

#[test]
fn parses_admin_auth_token() {
    let cfg = parse_str(
        r#"
settings:
  admin:
    listen: "127.0.0.1:9900"
    auth_token: "secret123"
pools:
  - name: p
    targets: ["127.0.0.1:9001"]
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    pool: p
"#,
    )
    .expect("should parse");
    assert_eq!(cfg.admin_auth_token.as_deref(), Some("secret123"));
}

#[test]
fn parses_admin_tls() {
    let cfg = parse_str(
        r#"
settings:
  admin:
    tls:
      cert: /etc/gsp/tls/fullchain.pem
      key: /etc/gsp/tls/privkey.pem
pools:
  - name: p
    targets: ["127.0.0.1:9001"]
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    pool: p
"#,
    )
    .expect("should parse");
    assert_eq!(
        cfg.admin_tls,
        Some(AdminTls {
            cert: "/etc/gsp/tls/fullchain.pem".into(),
            key: "/etc/gsp/tls/privkey.pem".into(),
            ..AdminTls::default()
        })
    );
    assert_eq!(parse_str(MINIMAL).unwrap().admin_tls, None);
}

#[test]
fn admin_tls_takes_handshake_limits() {
    let cfg = parse_str(
        "settings:\n  admin:\n    tls:\n      cert: /c.pem\n      key: /k.pem\n      max_pending: 100\n      max_pending_per_source: 4\n      new_per_source_per_sec: 0\n      new_per_source_burst: 10\npools:\n  - name: p\n    targets: [\"127.0.0.1:9001\"]\nlisteners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n",
    )
    .expect("should parse");
    let t = cfg.admin_tls.unwrap();
    assert_eq!(
        (
            t.max_pending,
            t.max_pending_per_source,
            t.new_per_source_per_sec,
            t.new_per_source_burst
        ),
        (Some(100), Some(4), Some(0.0), Some(10))
    );
}

#[test]
fn admin_tls_handshake_limits_are_validated() {
    for bad in [
        "max_pending: 0",
        "max_pending_per_source: 0",
        "new_per_source_burst: 0",
        "new_per_source_per_sec: -1",
        "new_per_source_per_sec: .nan",
    ] {
        let text = format!(
            "settings:\n  admin:\n    tls:\n      cert: /c.pem\n      key: /k.pem\n      {bad}\npools:\n  - name: p\n    targets: [\"127.0.0.1:9001\"]\nlisteners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n"
        );
        assert!(parse_str(&text).is_err(), "accepted:\n{text}");
    }
}

#[test]
fn admin_tls_needs_both_files() {
    for tls in [
        "cert: /c.pem",
        "key: /k.pem",
        "cert: /c.pem\n      key: /k.pem\n      ca: /x",
    ] {
        let text = format!(
            "settings:\n  admin:\n    tls:\n      {tls}\npools:\n  - name: p\n    targets: [\"127.0.0.1:9001\"]\nlisteners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n"
        );
        assert!(parse_str(&text).is_err(), "accepted:\n{text}");
    }
}

#[test]
fn parses_proxy_protocol_pool_option() {
    let cfg = parse_str(
        r#"
pools:
  - name: p
    targets: ["127.0.0.1:9001"]
    proxy_protocol: v2
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    pool: p
"#,
    )
    .expect("should parse");
    assert_eq!(cfg.pools[0].proxy_protocol, ProxyProtocol::V2);
}

#[test]
fn parses_v2_udp_on_a_udp_listener() {
    let cfg = parse_str(
        r#"
pools:
  - name: p
    targets: ["127.0.0.1:9001"]
    proxy_protocol: v2-udp
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    protocol: udp
    pool: p
"#,
    )
    .expect("should parse");
    assert_eq!(cfg.pools[0].proxy_protocol, ProxyProtocol::V2Udp);
}

#[test]
fn rejects_v2_udp_on_a_tcp_listener() {
    let err = parse_str(
        r#"
pools:
  - name: p
    targets: ["127.0.0.1:9001"]
    proxy_protocol: v2-udp
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    protocol: tcp
    pool: p
"#,
    );
    assert!(err.is_err());
}

#[test]
fn rejects_tcp_proxy_protocol_on_a_udp_listener() {
    let err = parse_str(
        r#"
pools:
  - name: p
    targets: ["127.0.0.1:9001"]
    proxy_protocol: v2
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    protocol: udp
    pool: p
"#,
    );
    assert!(err.is_err());
}

#[test]
fn rejects_unknown_proxy_protocol() {
    let err = parse_str(
        r#"
pools:
  - name: p
    targets: ["127.0.0.1:9001"]
    proxy_protocol: v3
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    pool: p
"#,
    );
    assert!(err.is_err());
}

#[test]
fn parses_health_check_and_caps() {
    let yaml = r#"
pools:
  - name: p
    targets: ["127.0.0.1:1", "127.0.0.1:2"]
    balancer: least_conn
    health_check: { interval_sec: 1, timeout_ms: 200, rise: 1, fall: 1 }
    per_backend: { max_sessions: 50 }
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    pool: p
"#;
    let cfg = parse_str(yaml).unwrap();
    assert_eq!(cfg.pools[0].balancer, Balancer::LeastConn);
    assert_eq!(cfg.pools[0].health_check.interval.as_secs(), 1);
    assert_eq!(cfg.pools[0].health_check.fall, 1);
    assert_eq!(cfg.pools[0].max_sessions, Some(50));
}

#[test]
fn parses_consistent_hash_balancer() {
    let yaml = r#"
pools:
  - name: a
    targets: ["127.0.0.1:1"]
    balancer: consistent_hash
  - name: b
    targets: ["127.0.0.1:2"]
    balancer: consistent_hash
    hash_on: src_ip_port
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    pool: a
"#;
    let cfg = parse_str(yaml).unwrap();
    assert_eq!(cfg.pools[0].balancer, Balancer::ConsistentHash);
    assert_eq!(cfg.pools[0].hash_on, Some(HashOn::SrcIp)); // default
    assert_eq!(cfg.pools[1].hash_on, Some(HashOn::SrcIpPort));
}

#[test]
fn rejects_hash_on_without_consistent_hash() {
    let yaml = r#"
pools:
  - name: p
    targets: ["127.0.0.1:1"]
    balancer: round_robin
    hash_on: src_ip
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    pool: p
"#;
    assert!(parse_str(yaml).is_err());
}

#[test]
fn parses_weighted_pool_and_rejects_bad_weights() {
    let yaml = r#"
pools:
  - name: p
    targets: ["127.0.0.1:1", "127.0.0.1:2"]
    balancer: weighted
    weights: { "127.0.0.1:1": 3, "127.0.0.1:2": 1 }
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    pool: p
"#;
    let cfg = parse_str(yaml).unwrap();
    assert_eq!(cfg.pools[0].balancer, Balancer::Weighted);
    assert_eq!(
        cfg.pools[0].weights.get(&"127.0.0.1:1".parse().unwrap()),
        Some(&3)
    );

    let bad = |extra: &str, bal: &str| {
        format!(
            "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n    balancer: {bal}\n    {extra}\nlisteners:\n  - {{ name: l, bind: \"0.0.0.0:7777\", pool: p }}\n"
        )
    };
    // weights with a non-weighted balancer
    assert!(parse_str(&bad(r#"weights: { "127.0.0.1:1": 2 }"#, "round_robin")).is_err());
    // weight of zero
    assert!(parse_str(&bad(r#"weights: { "127.0.0.1:1": 0 }"#, "weighted")).is_err());
    // weights key that isn't an ip:port
    assert!(parse_str(&bad(r#"weights: { "not-an-addr": 2 }"#, "weighted")).is_err());
}

#[test]
fn rejects_unknown_pool() {
    let yaml = r#"
pools:
  - name: local
    targets: ["127.0.0.1:9001"]
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    pool: nope
"#;
    let err = parse_str(yaml).unwrap_err();
    assert!(matches!(err, ConfigError::Invalid(_)), "got {err:?}");
}

#[test]
fn accepts_udp_listener_with_default_affinity() {
    let yaml = r#"
pools:
  - name: local
    targets: ["127.0.0.1:9001"]
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    protocol: udp
    pool: local
"#;
    let cfg = parse_str(yaml).unwrap();
    assert_eq!(cfg.listeners[0].protocol, Protocol::Udp);
    assert_eq!(cfg.listeners[0].affinity, Some(HashOn::SrcIp));
}

#[test]
fn rejects_affinity_on_tcp_listener() {
    let yaml = r#"
pools:
  - name: local
    targets: ["127.0.0.1:9001"]
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    protocol: tcp
    pool: local
    affinity: { hash_on: src_ip }
"#;
    assert!(parse_str(yaml).is_err());
}

#[test]
fn parses_udp_probe_health_check() {
    let yaml = r#"
pools:
  - name: p
    targets: ["127.0.0.1:1"]
    health_check: { type: udp_probe, send_hex: "ff ff ff ff", expect_hex_prefix: "ffff" }
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    protocol: udp
    pool: p
"#;
    let cfg = parse_str(yaml).unwrap();
    assert_eq!(
        cfg.pools[0].health_check.kind,
        HealthCheckKind::UdpProbe {
            send: vec![0xff, 0xff, 0xff, 0xff],
            expect_prefix: vec![0xff, 0xff],
        }
    );
}

#[test]
fn rejects_udp_probe_without_send_hex() {
    let yaml = r#"
pools:
  - name: p
    targets: ["127.0.0.1:1"]
    health_check: { type: udp_probe }
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    protocol: udp
    pool: p
"#;
    assert!(parse_str(yaml).is_err());
}

#[test]
fn rejects_zero_rise() {
    let yaml = r#"
pools:
  - name: p
    targets: ["127.0.0.1:1"]
    health_check: { rise: 0 }
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    pool: p
"#;
    assert!(parse_str(yaml).is_err());
}

#[test]
fn rejects_zero_max_sessions() {
    let yaml = r#"
pools:
  - name: p
    targets: ["127.0.0.1:1"]
    per_backend: { max_sessions: 0 }
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    pool: p
"#;
    assert!(parse_str(yaml).is_err());
}

#[test]
fn rejects_empty_targets() {
    let yaml = r#"
pools:
  - name: local
    targets: []
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    pool: local
"#;
    assert!(parse_str(yaml).is_err());
}

#[test]
fn rejects_no_listeners() {
    assert!(parse_str("pools: []").is_err());
}

#[test]
fn bare_pool_becomes_one_always_route() {
    let cfg = parse_str(MINIMAL).unwrap();
    assert_eq!(cfg.listeners[0].routes.len(), 1);
    assert_eq!(cfg.listeners[0].routes[0].matcher, Matcher::Always);
    assert_eq!(
        cfg.listeners[0].routes[0].action,
        Action::Pool("local".into())
    );
}

#[test]
fn parses_route_list_and_matches_first() {
    let yaml = r#"
pools:
  - name: staging
    targets: ["127.0.0.1:1"]
  - name: prod
    targets: ["127.0.0.1:2"]
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    routes:
      - match: { type: client_cidr, cidrs: ["10.0.0.0/8", "192.168.1.0/24"] }
        action: { pool: staging }
      - match: { type: port, ports: [7777, "8000-8100"] }
        action: { pool: prod }
      - match: { type: always }
        action: { pool: prod }
"#;
    let cfg = parse_str(yaml).unwrap();
    let l = &cfg.listeners[0];
    assert_eq!(l.routes.len(), 3);
    assert_eq!(l.peek_len(), 0);
    assert_eq!(
        l.route_for(&ctx("10.1.2.3:5555", "203.0.113.1:7777", &[])),
        Some("staging")
    );
    assert_eq!(
        l.route_for(&ctx("192.168.1.9:5555", "203.0.113.1:7777", &[])),
        Some("staging")
    );
    // no CIDR match, but destination port 7777 does
    assert_eq!(
        l.route_for(&ctx("203.0.113.9:5555", "203.0.113.1:7777", &[])),
        Some("prod")
    );
    // falls through to `always`
    assert_eq!(
        l.route_for(&ctx("203.0.113.9:5555", "203.0.113.1:9999", &[])),
        Some("prod")
    );
}

#[test]
fn dst_matcher_selects_by_destination_ip() {
    let yaml = r#"
pools:
  - name: survival
    targets: ["127.0.0.1:1"]
  - name: creative
    targets: ["127.0.0.1:2"]
  - name: lobby
    targets: ["127.0.0.1:3"]
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    routes:
      - match: { type: dst, cidrs: ["2001:db8:ace:1::1/128", "198.51.100.7/32"] }
        action: { pool: survival }
      - match: { type: dst, cidrs: ["2001:db8:ace:1::2/128"] }
        action: { pool: creative }
      - match: { type: always }
        action: { pool: lobby }
"#;
    let cfg = parse_str(yaml).unwrap();
    let l = &cfg.listeners[0];
    assert_eq!(l.peek_len(), 0);
    let on = |ip: &str| {
        l.route_for(&ctx("203.0.113.9:5555", &format!("{ip}:7777"), &[]))
            .map(str::to_string)
    };
    assert_eq!(on("198.51.100.7").as_deref(), Some("survival"));
    assert_eq!(on("[2001:db8:ace:1::1]").as_deref(), Some("survival"));
    assert_eq!(on("[2001:db8:ace:1::2]").as_deref(), Some("creative"));
    assert_eq!(on("198.51.100.9").as_deref(), Some("lobby"));
}

#[test]
fn rejects_dst_without_cidrs_and_cidrs_on_wrong_type() {
    for bad in [
        r#"routes: [{ match: { type: dst }, action: { pool: p } }]"#,
        r#"routes: [{ match: { type: dst, cidrs: [] }, action: { pool: p } }]"#,
        r#"routes: [{ match: { type: port, cidrs: ["10.0.0.0/8"] }, action: { pool: p } }]"#,
    ] {
        let yaml = format!(
            "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\nlisteners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    {bad}\n"
        );
        assert!(parse_str(&yaml).is_err(), "should reject: {bad}");
    }
}

#[test]
fn first_bytes_prefix_matches_and_sets_peek_len() {
    let yaml = r#"
pools:
  - name: query
    targets: ["127.0.0.1:1"]
  - name: game
    targets: ["127.0.0.1:2"]
listeners:
  - name: l
    bind: "0.0.0.0:27015"
    protocol: udp
    routes:
      - match: { type: first_bytes, prefix: "hex:ffffffff" }
        action: { pool: query }
      - match: { type: first_bytes, prefix: "ascii:GET " }
        action: { pool: query }
      - match: { type: always }
        action: { pool: game }
"#;
    let cfg = parse_str(yaml).unwrap();
    let l = &cfg.listeners[0];
    assert_eq!(l.peek_len(), 4);
    assert_eq!(
        l.route_for(&ctx(
            "1.2.3.4:5",
            "9.9.9.9:27015",
            &[0xff, 0xff, 0xff, 0xff, 0x54]
        )),
        Some("query")
    );
    assert_eq!(
        l.route_for(&ctx("1.2.3.4:5", "9.9.9.9:27015", b"GET /x")),
        Some("query")
    );
    assert_eq!(
        l.route_for(&ctx("1.2.3.4:5", "9.9.9.9:27015", b"\x01\x02random")),
        Some("game")
    );
    // nothing peeked yet -> prefix routes cannot match, falls through
    assert_eq!(
        l.route_for(&ctx("1.2.3.4:5", "9.9.9.9:27015", &[])),
        Some("game")
    );
}

#[test]
fn first_bytes_length_bound_matches_and_combines_with_prefix() {
    let yaml = r#"
pools:
  - name: q
    targets: ["127.0.0.1:1"]
  - name: g
    targets: ["127.0.0.1:2"]
listeners:
  - name: l
    bind: "0.0.0.0:27015"
    protocol: udp
    routes:
      - match: { type: first_bytes, prefix: "hex:ffff", length: { min: 4, max: 8 } }
        action: { pool: q }
      - match: { type: first_bytes, length: { min: 0, max: 15 } }
        action: { pool: q }
      - match: { type: always }
        action: { pool: g }
"#;
    let cfg = parse_str(yaml).unwrap();
    let l = &cfg.listeners[0];
    assert_eq!(l.peek_len(), 16); // max(prefix 2, rule1 8+1, rule2 15+1)
    let route = |b: &[u8]| l.route_for(&ctx("1.2.3.4:5", "9.9.9.9:27015", b));
    // rule 1: prefix ffff AND length 4..=8
    assert_eq!(route(&[0xff, 0xff, 0x01, 0x02, 0x03]), Some("q"));
    // prefix ok but length 2 out of 4..=8 -> rule 1 skipped, rule 2 (len<=15) hits
    assert_eq!(route(&[0xff, 0xff]), Some("q"));
    // length 20 > 15 and no ffff prefix -> nothing but `always`
    assert_eq!(route(&[0u8; 20]), Some("g"));
}

#[test]
fn rejects_bad_first_bytes_specs() {
    for bad in [
        r#"routes: [{ match: { type: first_bytes, prefix: "ffff" }, action: { pool: p } }]"#,
        r#"routes: [{ match: { type: first_bytes }, action: { pool: p } }]"#,
        r#"routes: [{ match: { type: first_bytes, prefix: "hex:" }, action: { pool: p } }]"#,
        r#"routes: [{ match: { type: first_bytes, length: { min: 9, max: 4 } }, action: { pool: p } }]"#,
        r#"routes: [{ match: { type: always, prefix: "hex:ff" }, action: { pool: p } }]"#,
        r#"routes: [{ match: { type: port, length: { min: 0, max: 4 } }, action: { pool: p } }]"#,
    ] {
        let yaml = format!(
            "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\nlisteners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    {bad}\n"
        );
        assert!(parse_str(&yaml).is_err(), "should reject: {bad}");
    }
}

/// A minimal TLS ClientHello record carrying `sni` in the SNI extension.
fn client_hello(sni: &str) -> Vec<u8> {
    let mut sn = Vec::new();
    sn.extend_from_slice(&((sni.len() + 3) as u16).to_be_bytes()); // list len
    sn.push(0x00); // host_name
    sn.extend_from_slice(&(sni.len() as u16).to_be_bytes());
    sn.extend_from_slice(sni.as_bytes());

    let mut ext = Vec::new();
    ext.extend_from_slice(&0u16.to_be_bytes()); // server_name
    ext.extend_from_slice(&(sn.len() as u16).to_be_bytes());
    ext.extend_from_slice(&sn);

    let mut body = Vec::new();
    body.extend_from_slice(&[0x03, 0x03]); // client_version
    body.extend_from_slice(&[0u8; 32]); // random
    body.push(0x00); // session_id len
    body.extend_from_slice(&2u16.to_be_bytes()); // cipher_suites
    body.extend_from_slice(&[0x00, 0x2f]);
    body.push(0x01); // compression methods
    body.push(0x00);
    body.extend_from_slice(&(ext.len() as u16).to_be_bytes());
    body.extend_from_slice(&ext);

    let bl = body.len();
    let mut hs = vec![0x01, (bl >> 16) as u8, (bl >> 8) as u8, bl as u8];
    hs.extend_from_slice(&body);

    let mut rec = vec![0x16, 0x03, 0x01];
    rec.extend_from_slice(&(hs.len() as u16).to_be_bytes());
    rec.extend_from_slice(&hs);
    rec
}

#[test]
fn extract_sni_reads_the_client_hello() {
    assert_eq!(
        extract_sni(&client_hello("EU.Example.COM")).as_deref(),
        Some("eu.example.com")
    );
    assert_eq!(extract_sni(b""), None);
    assert_eq!(extract_sni(b"\x16\x03\x01\x00\x05hello"), None); // truncated
    assert_eq!(extract_sni(&[0u8; 200]), None); // not a handshake
}

#[test]
fn sni_matcher_exact_and_suffix() {
    let yaml = r#"
pools:
  - name: eu
    targets: ["127.0.0.1:1"]
  - name: lobby
    targets: ["127.0.0.1:2"]
listeners:
  - name: l
    bind: "0.0.0.0:443"
    routes:
      - match: { type: sni, host: ["*.eu.example.com", "special.example.net"] }
        action: { pool: eu }
      - match: { type: always }
        action: { pool: lobby }
"#;
    let cfg = parse_str(yaml).unwrap();
    let l = &cfg.listeners[0];
    assert_eq!(l.peek_len(), PEEK_MAX);
    let route = |sni: &str| {
        let ch = client_hello(sni);
        l.route_for(&ctx("9.9.9.9:5", "1.1.1.1:443", &ch))
            .map(str::to_string)
    };
    assert_eq!(route("a.eu.example.com").as_deref(), Some("eu"));
    assert_eq!(route("x.y.eu.example.com").as_deref(), Some("eu"));
    assert_eq!(route("special.example.net").as_deref(), Some("eu"));
    assert_eq!(route("eu.example.com").as_deref(), Some("lobby")); // suffix != apex
    assert_eq!(route("us.example.com").as_deref(), Some("lobby"));
    // no ClientHello at all -> sni route can't match
    assert_eq!(
        l.route_for(&ctx("9.9.9.9:5", "1.1.1.1:443", b"not tls")),
        Some("lobby")
    );
}

#[test]
fn rejects_bad_sni_matchers() {
    for bad in [
        // sni on a udp listener
        "  - name: l\n    bind: \"0.0.0.0:443\"\n    protocol: udp\n    routes: [{ match: { type: sni, host: [\"a.example.com\"] }, action: { pool: p } }]",
        // empty host list
        "  - name: l\n    bind: \"0.0.0.0:443\"\n    routes: [{ match: { type: sni, host: [] }, action: { pool: p } }]",
        // '*' not as a leading label
        "  - name: l\n    bind: \"0.0.0.0:443\"\n    routes: [{ match: { type: sni, host: [\"a*b.example.com\"] }, action: { pool: p } }]",
        // wrong field
        "  - name: l\n    bind: \"0.0.0.0:443\"\n    routes: [{ match: { type: sni, cidrs: [\"10.0.0.0/8\"] }, action: { pool: p } }]",
    ] {
        let yaml = format!("pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\nlisteners:\n{bad}\n");
        assert!(parse_str(&yaml).is_err(), "should reject: {bad}");
    }
}

#[test]
fn parses_sniffer_matcher_and_hints_the_listener() {
    let yaml = r#"
pools:
  - name: survival
    targets: ["127.0.0.1:1"]
  - name: lobby
    targets: ["127.0.0.1:2"]
listeners:
  - name: l
    bind: "0.0.0.0:25565"
    routes:
      - match: { type: sniffer, sniffer: minecraft, host: ["survival.example.net"] }
        action: { pool: survival }
      - match: { type: sniffer, sniffer: minecraft }
        action: { pool: lobby }
      - match: { type: always }
        action: { pool: lobby }
"#;
    let cfg = parse_str(yaml).unwrap();
    let l = &cfg.listeners[0];
    assert_eq!(l.sniffers, ["minecraft"]);
    assert_eq!(l.peek_len(), PEEK_MAX);

    let hint = |host: Option<&str>, reject: bool| RouteHint {
        host: host.map(str::to_string),
        key: None,
        reject,
    };
    // exact host -> survival
    assert_eq!(
        l.route_for(&ctx_sniff(&hint(Some("survival.example.net"), false))),
        Some("survival")
    );
    // recognised but different host -> the bare `sniffer` rule (lobby)
    assert_eq!(
        l.route_for(&ctx_sniff(&hint(Some("creative.example.net"), false))),
        Some("lobby")
    );
    // recognised, no host -> bare `sniffer` rule
    assert_eq!(l.route_for(&ctx_sniff(&hint(None, false))), Some("lobby"));
    // reject -> neither sniffer rule matches, falls to `always`
    assert_eq!(
        l.route_for(&ctx_sniff(&hint(Some("survival.example.net"), true))),
        Some("lobby")
    );
    // no hint at all -> falls to `always`
    assert_eq!(
        l.route_for(&ctx("9.9.9.9:5", "1.1.1.1:25565", &[])),
        Some("lobby")
    );
}

#[test]
fn rejects_bad_sniffer_matchers() {
    for bad in [
        // missing sniffer name
        r#"routes: [{ match: { type: sniffer, host: ["a.example.com"] }, action: { pool: p } }]"#,
        // sniffer field on a non-sniffer matcher
        r#"routes: [{ match: { type: always, sniffer: sni }, action: { pool: p } }]"#,
    ] {
        let yaml = format!(
            "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\nlisteners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    {bad}\n"
        );
        assert!(parse_str(&yaml).is_err(), "should reject: {bad}");
    }
}

#[test]
fn several_sniffers_per_listener_route_by_the_recognising_one() {
    let yaml = r#"
pools:
  - name: quic
    targets: ["127.0.0.1:1"]
  - name: wg
    targets: ["127.0.0.1:2"]
  - name: other
    targets: ["127.0.0.1:3"]
listeners:
  - name: l
    bind: "0.0.0.0:443"
    protocol: udp
    routes:
      - match: { type: sniffer, sniffer: quic }
        action: { pool: quic }
      - match: { type: sniffer, sniffer: wireguard }
        action: { pool: wg }
      - match: { type: sniffer, sniffer: quic, host: ["x.example.net"] }
        action: { pool: other }
"#;
    let l = &parse_str(yaml).unwrap().listeners[0];
    // distinct, in order of first appearance
    assert_eq!(l.sniffers, ["quic", "wireguard"]);
    let h = RouteHint::default();
    assert_eq!(l.route_for(&ctx_sniff_by("quic", &h)), Some("quic"));
    assert_eq!(l.route_for(&ctx_sniff_by("wireguard", &h)), Some("wg"));
    // a hit from a sniffer no route names matches nothing
    assert_eq!(l.route_for(&ctx_sniff_by("a2s", &h)), None);
    // a wireguard hit never satisfies the quic host route
    let hh = RouteHint {
        host: Some("x.example.net".into()),
        ..Default::default()
    };
    assert_eq!(l.route_for(&ctx_sniff_by("wireguard", &hh)), Some("wg"));
    assert!(l.first_packet_recognised(&ctx_sniff_by("wireguard", &h)));
}

#[test]
fn parses_udp_prefix_listener() {
    let yaml = r#"
pools:
  - name: p
    targets: ["127.0.0.1:1"]
listeners:
  - name: l
    bind: "[::]:7777"
    protocol: udp
    prefix: "2001:db8:ace:1::/64"
    routes:
      - match: { type: dst, cidrs: ["2001:db8:ace:1::1/128"] }
        action: { pool: p }
      - match: { type: always }
        action: { pool: p }
"#;
    let cfg = parse_str(yaml).unwrap();
    let l = &cfg.listeners[0];
    let prefix = l.prefix.as_ref().unwrap();
    assert!(prefix.contains("2001:db8:ace:1::1".parse().unwrap()));
    assert!(!prefix.contains("2001:db8:ace:2::1".parse().unwrap()));
    assert!(!l.freebind);
}

#[test]
fn rejects_bad_prefix_and_freebind() {
    for bad in [
        // prefix on a tcp listener
        "  - name: l\n    bind: \"[::]:7777\"\n    protocol: tcp\n    prefix: \"2001:db8::/64\"\n    pool: p",
        // prefix with a non-wildcard bind
        "  - name: l\n    bind: \"[2001:db8::1]:7777\"\n    protocol: udp\n    prefix: \"2001:db8::/64\"\n    pool: p",
        // unparseable prefix
        "  - name: l\n    bind: \"[::]:7777\"\n    protocol: udp\n    prefix: \"nonsense\"\n    pool: p",
        // freebind on a udp listener
        "  - name: l\n    bind: \"0.0.0.0:7777\"\n    protocol: udp\n    freebind: true\n    pool: p",
        // transparent + prefix together
        "  - name: l\n    bind: \"[::]:7777\"\n    protocol: udp\n    transparent: true\n    prefix: \"2001:db8::/64\"\n    pool: p",
    ] {
        let yaml = format!("pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\nlisteners:\n{bad}\n");
        assert!(parse_str(&yaml).is_err(), "should reject: {bad}");
    }
}

#[test]
fn accepts_tcp_freebind_listener() {
    let yaml = r#"
pools:
  - name: p
    targets: ["127.0.0.1:1"]
listeners:
  - name: l
    bind: "198.51.100.7:443"
    freebind: true
    pool: p
"#;
    let cfg = parse_str(yaml).unwrap();
    assert!(cfg.listeners[0].freebind);
    assert!(cfg.listeners[0].prefix.is_none());
    assert!(!cfg.listeners[0].route_hint);
    assert!(!cfg.listeners[0].transparent);
}

#[test]
fn parses_bind_port_range_listener() {
    let yaml = r#"
pools:
  - name: p
    targets: ["127.0.0.1:1"]
listeners:
  - name: l
    bind: "0.0.0.0:30000-30099"
    protocol: udp
    routes:
      - match: { type: port, ports: [30001] }
        action: { pool: p }
      - match: { type: always }
        action: { pool: p }
"#;
    let cfg = parse_str(yaml).unwrap();
    let l = &cfg.listeners[0];
    assert_eq!(l.bind, "0.0.0.0:30000".parse::<SocketAddr>().unwrap());
    assert_eq!(l.extra_binds.len(), 99);
    assert_eq!(
        l.extra_binds.last().copied().unwrap(),
        "0.0.0.0:30099".parse::<SocketAddr>().unwrap()
    );
    assert_eq!(l.binds().count(), 100);
    assert!(l.binds().all(|a| a.ip() == std::net::Ipv4Addr::UNSPECIFIED));
}

#[test]
fn parses_bind_port_range_ipv6() {
    let yaml = r#"
pools:
  - name: p
    targets: ["127.0.0.1:1"]
listeners:
  - name: l
    bind: "[2001:db8::1]:7000-7001"
    protocol: tcp
    pool: p
"#;
    let cfg = parse_str(yaml).unwrap();
    let l = &cfg.listeners[0];
    assert_eq!(l.bind, "[2001:db8::1]:7000".parse::<SocketAddr>().unwrap());
    assert_eq!(
        l.extra_binds,
        vec!["[2001:db8::1]:7001".parse::<SocketAddr>().unwrap()]
    );
}

#[test]
fn bind_range_overlap_is_rejected_like_a_plain_bind() {
    // A range that overlaps another listener's single bind is rejected,
    // same as two plain binds colliding.
    let yaml = "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
                 listeners:\n  \
                 - name: a\n    bind: \"0.0.0.0:30050\"\n    pool: p\n  \
                 - name: b\n    bind: \"0.0.0.0:30000-30099\"\n    pool: p\n";
    assert!(parse_str(yaml).is_err());
}

#[test]
fn rejects_bad_bind_ranges() {
    for bad in [
        // reversed range
        "0.0.0.0:30099-30000",
        // port 0 in range
        "0.0.0.0:0-10",
        // non-numeric bound
        "0.0.0.0:abc-30099",
        // way over the sanity cap
        "0.0.0.0:1-65000",
        // bad host
        "not-an-ip:30000-30099",
    ] {
        let yaml = format!(
            "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
             listeners:\n  - name: l\n    bind: {bad:?}\n    pool: p\n"
        );
        assert!(parse_str(&yaml).is_err(), "should reject bind: {bad}");
    }
}

#[test]
fn rejects_bind_range_combined_with_prefix() {
    let yaml = "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
                 listeners:\n  - name: l\n    bind: \"[::]:7777-7778\"\n    \
                 protocol: udp\n    prefix: \"2001:db8::/64\"\n    pool: p\n";
    assert!(parse_str(yaml).is_err());
}

#[test]
fn parses_global_limits_and_rejects_zero() {
    let yaml = "settings:\n  limits:\n    max_connections: 5000\n    \
                max_new_sessions_per_sec: 200\n\
                pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
                listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n";
    let lim = parse_str(yaml).unwrap().limits;
    assert_eq!(lim.max_connections, Some(5000));
    assert_eq!(lim.max_udp_sessions, None);
    assert_eq!(lim.max_new_sessions_per_sec, Some(200));

    let bad = "settings:\n  limits:\n    max_udp_sessions: 0\n\
               pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
               listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n";
    assert!(parse_str(bad).is_err());
}

#[test]
fn absent_global_limits_are_empty() {
    let yaml = "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
                listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n";
    assert!(parse_str(yaml).unwrap().limits.is_empty());
}

#[test]
fn parses_geo_filter_and_uppercases_codes() {
    let yaml = r#"
settings:
  geo_db: "/tmp/whatever.mmdb"
pools:
  - name: p
    targets: ["127.0.0.1:1"]
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    pool: p
    geo:
      allow: ["se"]
      deny: ["Gb", "RU"]
"#;
    let cfg = parse_str(yaml).unwrap();
    assert_eq!(cfg.geo_db.as_deref(), Some("/tmp/whatever.mmdb"));
    let geo = cfg.listeners[0].geo.clone().unwrap();
    assert_eq!(geo.allow, vec![*b"SE"]);
    assert_eq!(geo.deny, vec![*b"GB", *b"RU"]);

    // deny wins; non-empty allow is default-deny; an unknown country is
    // admitted only with no allow list.
    assert!(geo.permits(Some(*b"SE")));
    assert!(!geo.permits(Some(*b"GB")));
    assert!(!geo.permits(Some(*b"FR"))); // not in allow
    assert!(!geo.permits(None)); // allow present -> fail closed
    let deny_only = GeoAcl {
        allow: vec![],
        deny: vec![*b"GB"],
    };
    assert!(deny_only.permits(None));
    assert!(deny_only.permits(Some(*b"FR")));
    assert!(!deny_only.permits(Some(*b"GB")));
}

#[test]
fn rejects_bad_geo_filter() {
    for bad in [
        // geo without settings.geo_db
        "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
         listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n    \
         geo:\n      deny: [\"GB\"]",
        // empty geo
        "settings:\n  geo_db: \"/x.mmdb\"\n\
         pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
         listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n    \
         geo: {}",
        // not a 2-letter code
        "settings:\n  geo_db: \"/x.mmdb\"\n\
         pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
         listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n    \
         geo:\n      deny: [\"GBR\"]",
    ] {
        assert!(parse_str(bad).is_err(), "should reject: {bad}");
    }
}

#[test]
fn parses_sniffers_settings_with_defaults_and_pins() {
    let yaml = "settings:\n  sniffers:\n    dir: \"/etc/gsp/sniffers\"\n\
                pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
                listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n";
    let s = parse_str(yaml).unwrap().sniffers.unwrap();
    assert_eq!(s.dir, "/etc/gsp/sniffers");
    assert_eq!(s.call_timeout, Duration::from_millis(20));
    assert_eq!(s.max_memory_bytes, 16 * 1024 * 1024);
    assert!(s.modules.is_empty());

    let yaml = format!(
        "settings:\n  sniffers:\n    dir: \"/plugins\"\n    call_timeout_ms: 5\n    \
         max_memory_bytes: 1048576\n    modules:\n      - name: a2s\n        sha256: \"{}\"\n      \
         - name: regex_firstbytes\n        sha256: \"{}\"\n        config: \"^GET \"\n\
         pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
         listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n",
        "AB".repeat(32),
        "CD".repeat(32),
    );
    let s = parse_str(&yaml).unwrap().sniffers.unwrap();
    assert_eq!(s.call_timeout, Duration::from_millis(5));
    assert_eq!(s.max_memory_bytes, 1_048_576);
    assert_eq!(s.modules.len(), 2);
    assert_eq!(s.modules[0].name, "a2s");
    assert_eq!(s.modules[0].sha256, "ab".repeat(32)); // lower-cased
    assert_eq!(s.modules[0].config, None);
    assert_eq!(s.modules[1].config.as_deref(), Some("^GET "));
}

#[test]
fn absent_sniffers_settings_load_no_plugins() {
    let yaml = "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
                listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n";
    assert!(parse_str(yaml).unwrap().sniffers.is_none());
}

#[test]
fn rejects_bad_sniffers_settings() {
    for bad in [
        // empty dir
        "settings:\n  sniffers:\n    dir: \"\"\n\
         pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
         listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n",
        // zero call_timeout_ms
        "settings:\n  sniffers:\n    dir: \"/x\"\n    call_timeout_ms: 0\n\
         pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
         listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n",
        // zero max_memory_bytes
        "settings:\n  sniffers:\n    dir: \"/x\"\n    max_memory_bytes: 0\n\
         pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
         listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n",
        // empty module name
        "settings:\n  sniffers:\n    dir: \"/x\"\n    modules:\n      - name: \"\"\n        sha256: \"ab\"\n\
         pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
         listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n",
        // bad sha256 (too short, not hex)
        "settings:\n  sniffers:\n    dir: \"/x\"\n    modules:\n      - name: a2s\n        sha256: \"zz\"\n\
         pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
         listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n",
        // unknown field
        "settings:\n  sniffers:\n    dir: \"/x\"\n    bogus: 1\n\
         pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
         listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n",
    ] {
        assert!(parse_str(bad).is_err(), "should reject: {bad}");
    }

    // empty `config` string on a module
    let bad = format!(
        "settings:\n  sniffers:\n    dir: \"/x\"\n    modules:\n      - name: a2s\n        sha256: \"{}\"\n        config: \"\"\n\
         pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
         listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n",
        "ab".repeat(32),
    );
    assert!(parse_str(&bad).is_err(), "empty config must be rejected");
}

#[test]
fn parses_per_source_cap_and_rejects_bad() {
    let yaml = "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
                listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n    \
                per_source:\n      max_per_ip: 50\n      max_per_net: 500\n";
    let ps = parse_str(yaml).unwrap().listeners[0].per_source.unwrap();
    assert_eq!(ps.max_per_ip, Some(50));
    assert_eq!(ps.max_per_net, Some(500));

    for bad in [
        // empty
        "    per_source: {}",
        // zero
        "    per_source:\n      max_per_ip: 0",
    ] {
        let y = format!(
            "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
             listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n{bad}\n"
        );
        assert!(parse_str(&y).is_err(), "should reject: {bad}");
    }
}

#[test]
fn first_packet_gate_parses_and_recognises_known_first_bytes() {
    let yaml = r#"
pools:
  - name: p
    targets: ["127.0.0.1:1"]
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    protocol: udp
    first_packet_gate: true
    routes:
      - match: { type: first_bytes, prefix: "hex:ffffffff" }
        action: { pool: p }
      - match: { type: always }
        action: { pool: p }
"#;
    let l = &parse_str(yaml).unwrap().listeners[0];
    assert!(l.first_packet_gate);
    let ctx = |b: &'static [u8]| MatchContext {
        src: "1.2.3.4:5".parse().unwrap(),
        local: "9.9.9.9:7777".parse().unwrap(),
        first_bytes: b,
        sniff: None,
    };
    assert!(l.first_packet_recognised(&ctx(&[0xff, 0xff, 0xff, 0xff, 0x01])));
    assert!(!l.first_packet_recognised(&ctx(b"random junk")));
}

#[test]
fn rejects_bad_first_packet_gate() {
    for bad in [
        // tcp listener
        "  - name: l\n    bind: \"0.0.0.0:7777\"\n    protocol: tcp\n    first_packet_gate: true\n    pool: p",
        // udp but nothing to gate on
        "  - name: l\n    bind: \"0.0.0.0:7777\"\n    protocol: udp\n    first_packet_gate: true\n    pool: p",
    ] {
        let yaml = format!(
            "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\nlisteners:\n{bad}\n"
        );
        assert!(parse_str(&yaml).is_err(), "should reject: {bad}");
    }
}

#[test]
fn parses_and_applies_listener_acl() {
    let yaml = r#"
pools:
  - name: p
    targets: ["127.0.0.1:1"]
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    pool: p
    allow: ["10.0.0.0/8", "192.168.0.0/16"]
    deny: ["10.6.6.0/24"]
"#;
    let acl = &parse_str(yaml).unwrap().listeners[0].acl;
    assert!(!acl.is_empty());
    assert!(acl.permits("10.1.2.3".parse().unwrap()));
    assert!(acl.permits("192.168.9.9".parse().unwrap()));
    // deny wins over an allow match
    assert!(!acl.permits("10.6.6.6".parse().unwrap()));
    // non-empty allow ⇒ default-deny for anything uncovered
    assert!(!acl.permits("8.8.8.8".parse().unwrap()));
}

#[test]
fn deny_only_acl_is_default_allow() {
    let yaml = "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
                listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n    \
                deny: [\"203.0.113.0/24\"]\n";
    let acl = &parse_str(yaml).unwrap().listeners[0].acl;
    assert!(!acl.permits("203.0.113.5".parse().unwrap()));
    assert!(acl.permits("8.8.8.8".parse().unwrap()));
}

#[test]
fn absent_acl_is_empty_and_permits_all() {
    let yaml = "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
                listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n";
    let acl = &parse_str(yaml).unwrap().listeners[0].acl;
    assert!(acl.is_empty());
    assert!(acl.permits("8.8.8.8".parse().unwrap()));
}

#[test]
fn rejects_bad_acl_cidr() {
    let yaml = "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
                listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n    \
                deny: [\"not-a-cidr\"]\n";
    assert!(parse_str(yaml).is_err());
}

#[test]
fn parses_listener_rate_limit_with_burst_default() {
    let yaml = r#"
pools:
  - name: p
    targets: ["127.0.0.1:1"]
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    pool: p
    rate_limit:
      per_ip: { rate: 50 }
      per_net: { rate: 500, burst: 800 }
"#;
    let rl = parse_str(yaml).unwrap().listeners[0].rate_limit.unwrap();
    assert_eq!(
        rl.per_ip.unwrap(),
        TokenBucket {
            rate: 50,
            burst: 50
        }
    );
    assert_eq!(
        rl.per_net.unwrap(),
        TokenBucket {
            rate: 500,
            burst: 800
        }
    );
}

#[test]
fn rejects_bad_rate_limit() {
    for bad in [
        // no bucket at all
        "    rate_limit: {}",
        // zero rate
        "    rate_limit:\n      per_ip: { rate: 0 }",
    ] {
        let yaml = format!(
            "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
             listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n{bad}\n"
        );
        assert!(parse_str(&yaml).is_err(), "should reject: {bad}");
    }
}

#[test]
fn absent_rate_limit_is_none() {
    let yaml = "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
                listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n";
    assert!(parse_str(yaml).unwrap().listeners[0].rate_limit.is_none());
}

#[test]
fn parses_tcp_transparent_listener() {
    let yaml = r#"
pools:
  - name: p
    targets: ["127.0.0.1:1"]
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    protocol: tcp
    transparent: true
    pool: p
"#;
    let cfg = parse_str(yaml).unwrap();
    assert!(cfg.listeners[0].transparent);
}

#[test]
fn transparent_is_rejected_on_a_listener_reaching_a_tunnel_pool() {
    let key = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
    let head = format!(
        "backend_sources:\n  - {{ name: t, type: tunnel, pubkey: \"{key}\" }}\n\
         pools:\n  - {{ name: tp, source: t }}\n  - {{ name: sp, targets: [\"127.0.0.1:1\"] }}\n"
    );
    // Directly via `pool:`, and through a route action.
    for l in [
        "  - { name: l, bind: \"0.0.0.0:7777\", transparent: true, pool: tp }\n",
        "  - name: l\n    bind: \"0.0.0.0:7777\"\n    transparent: true\n    routes:\n      \
         - { match: { type: sni, host: [\"a.example\"] }, action: { pool: tp } }\n      \
         - { match: { type: always }, action: { pool: sp } }\n",
    ] {
        let err = parse_str(&format!("{head}listeners:\n{l}")).unwrap_err();
        assert!(
            err.to_string().contains("transparent") && err.to_string().contains("tunnel"),
            "{err}"
        );
    }
    // A tunnel pool without `transparent`, and `transparent` on a static pool, still load.
    parse_str(&format!(
        "{head}listeners:\n  - {{ name: l, bind: \"0.0.0.0:7777\", pool: tp }}\n"
    ))
    .unwrap();
    parse_str(&format!(
        "{head}listeners:\n  - {{ name: l, bind: \"0.0.0.0:7777\", transparent: true, pool: sp }}\n"
    ))
    .unwrap();
}

#[test]
fn parses_udp_transparent_listener() {
    let cfg = parse_str(
        "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
         listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    protocol: udp\n    \
         transparent: true\n    pool: p\n",
    )
    .unwrap();
    assert!(cfg.listeners[0].transparent);
}

#[test]
fn parses_route_hint_listener_flag() {
    let yaml = r#"
pools:
  - name: p
    targets: ["127.0.0.1:1"]
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    route_hint: true
    pool: p
"#;
    assert!(parse_str(yaml).unwrap().listeners[0].route_hint);
}

#[test]
fn parses_resolver_and_resolver_route_action() {
    let yaml = r#"
pools:
  - name: lobby
    targets: ["127.0.0.1:1"]
resolvers:
  - name: matchmaker
    type: http
    endpoint: "https://mm.internal:8443/resolve"
    timeout_ms: 25
    on_error: fallback_route
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    routes:
      - match: { type: always }
        action: { resolver: matchmaker }
      - match: { type: always }
        action: { pool: lobby }
"#;
    let cfg = parse_str(yaml).unwrap();
    assert_eq!(cfg.resolvers.len(), 1);
    assert_eq!(cfg.resolvers[0].kind, ResolverKind::Http);
    assert_eq!(cfg.resolvers[0].timeout.as_millis(), 25);
    assert_eq!(cfg.resolvers[0].on_error, OnError::FallbackRoute);
    assert_eq!(
        cfg.listeners[0].routes[0].action,
        Action::Resolver("matchmaker".into())
    );
    // a resolver route forces a full peek so the resolver can see the bytes
    assert_eq!(cfg.listeners[0].peek_len(), PEEK_MAX);
    // default: no PROXY header for a target
    assert_eq!(cfg.resolvers[0].proxy_protocol, ProxyProtocol::None);
}

#[test]
fn parses_resolver_target_proxy_protocol() {
    let yaml = r#"
pools: [{ name: p, targets: ["127.0.0.1:1"] }]
resolvers:
  - name: mm
    endpoint: "http://x"
    proxy_protocol: v2
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    routes:
      - { match: { type: always }, action: { resolver: mm } }
      - { match: { type: always }, action: { pool: p } }
"#;
    assert_eq!(
        parse_str(yaml).unwrap().resolvers[0].proxy_protocol,
        ProxyProtocol::V2
    );
}

#[test]
fn rejects_v2_udp_resolver_on_a_tcp_listener() {
    let yaml = r#"
pools: [{ name: p, targets: ["127.0.0.1:1"] }]
resolvers:
  - name: mm
    endpoint: "http://x"
    proxy_protocol: v2-udp
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    routes:
      - { match: { type: always }, action: { resolver: mm } }
      - { match: { type: always }, action: { pool: p } }
"#;
    assert!(parse_str(yaml).is_err());
}

#[test]
fn rejects_tcp_proxy_protocol_resolver_on_a_udp_listener() {
    let yaml = r#"
pools: [{ name: p, targets: ["127.0.0.1:1"] }]
resolvers:
  - name: mm
    endpoint: "http://x"
    proxy_protocol: v2
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    protocol: udp
    routes:
      - { match: { type: always }, action: { resolver: mm } }
      - { match: { type: always }, action: { pool: p } }
"#;
    assert!(parse_str(yaml).is_err());
}

#[test]
fn parses_resolver_cache() {
    let yaml = r#"
pools: [{ name: p, targets: ["127.0.0.1:1"] }]
resolvers:
  - name: mm
    endpoint: "http://x"
    cache:
      key: ["src_ip", "sni", "first_bytes:0:16"]
      positive_ttl_sec: 60
      negative_ttl_sec: 5
      max_entries: 1000
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    pool: p
"#;
    let cfg = parse_str(yaml).unwrap();
    let c = cfg.resolvers[0].cache.as_ref().unwrap();
    assert_eq!(
        c.key,
        vec![
            CacheKeyPart::SrcIp,
            CacheKeyPart::Sni,
            CacheKeyPart::FirstBytes(0..=16)
        ]
    );
    assert_eq!(c.positive_ttl.as_secs(), 60);
    assert_eq!(c.negative_ttl.as_secs(), 5);
    assert_eq!(c.max_entries, 1000);
}

#[test]
fn rejects_bad_resolver_cache() {
    for bad in [
        r#"cache: { key: [] }"#,
        r#"cache: { key: ["nonsense"] }"#,
        r#"cache: { key: ["first_bytes:8:4"] }"#,
        r#"cache: { key: ["first_bytes:0:99999"] }"#,
        r#"cache: { key: ["src_ip"], max_entries: 0 }"#,
    ] {
        let yaml = format!(
            "pools: [{{ name: p, targets: [\"127.0.0.1:1\"] }}]\nresolvers:\n  - {{ name: mm, endpoint: \"http://x\", {bad} }}\nlisteners:\n  - {{ name: l, bind: \"0.0.0.0:7777\", pool: p }}\n"
        );
        assert!(parse_str(&yaml).is_err(), "should reject: {bad}");
    }
}

#[test]
fn rejects_bad_resolver_and_actions() {
    for bad in [
        // route references an undefined resolver
        "resolvers: []\nlisteners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    routes: [{ match: { type: always }, action: { resolver: nope } }]",
        // action with both pool and resolver
        "resolvers:\n  - { name: r, endpoint: \"http://x\" }\nlisteners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    routes: [{ match: { type: always }, action: { pool: p, resolver: r } }]",
        // action with neither
        "listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    routes: [{ match: { type: always }, action: {} }]",
        // unknown transport
        "resolvers:\n  - { name: r, type: smoke, endpoint: \"http://x\" }\nlisteners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p",
        // empty endpoint
        "resolvers:\n  - { name: r, endpoint: \"\" }\nlisteners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p",
    ] {
        let yaml = format!("pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n{bad}\n");
        assert!(parse_str(&yaml).is_err(), "should reject: {bad}");
    }
}

#[test]
fn parses_resolver_target_timeouts_and_rejects_zero() {
    let with_resolver = |extra: &str| {
        format!(
            "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\nresolvers:\n  - {{ name: mm, endpoint: \"http://x\"{extra} }}\nlisteners:\n  - {{ name: l, bind: \"0.0.0.0:7777\", pool: p }}\n"
        )
    };

    // Defaults when omitted.
    let cfg = parse_str(&with_resolver("")).unwrap();
    assert_eq!(cfg.resolvers[0].target_connect_timeout.as_millis(), 300);
    assert_eq!(cfg.resolvers[0].target_idle_timeout.as_secs(), 90);

    // Explicit values parse.
    let cfg = parse_str(&with_resolver(
        ", target_connect_timeout_ms: 750, target_idle_timeout_sec: 20",
    ))
    .unwrap();
    assert_eq!(cfg.resolvers[0].target_connect_timeout.as_millis(), 750);
    assert_eq!(cfg.resolvers[0].target_idle_timeout.as_secs(), 20);

    // Zero for either is rejected.
    for bad in [
        ", target_connect_timeout_ms: 0",
        ", target_idle_timeout_sec: 0",
    ] {
        assert!(
            parse_str(&with_resolver(bad)).is_err(),
            "should reject: {bad}"
        );
    }
}

#[test]
fn rejects_pool_and_routes_together() {
    let yaml = r#"
pools:
  - name: p
    targets: ["127.0.0.1:1"]
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    pool: p
    routes:
      - match: { type: always }
        action: { pool: p }
"#;
    assert!(parse_str(yaml).is_err());
}

#[test]
fn rejects_route_to_unknown_pool() {
    let yaml = r#"
pools:
  - name: p
    targets: ["127.0.0.1:1"]
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    routes:
      - match: { type: always }
        action: { pool: nope }
"#;
    assert!(parse_str(yaml).is_err());
}

#[test]
fn rejects_always_matcher_with_fields() {
    let yaml = r#"
pools:
  - name: p
    targets: ["127.0.0.1:1"]
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    routes:
      - match: { type: always, cidrs: ["10.0.0.0/8"] }
        action: { pool: p }
"#;
    assert!(parse_str(yaml).is_err());
}

#[test]
fn rejects_bad_cidr_and_reversed_port_range() {
    for bad in [
        r#"routes: [{ match: { type: client_cidr, cidrs: ["10.0.0.0/33"] }, action: { pool: p } }]"#,
        r#"routes: [{ match: { type: port, ports: ["9000-8000"] }, action: { pool: p } }]"#,
        r#"routes: [{ match: { type: client_cidr }, action: { pool: p } }]"#,
    ] {
        let yaml = format!(
            "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\nlisteners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    {bad}\n"
        );
        assert!(parse_str(&yaml).is_err(), "should reject: {bad}");
    }
}

// -----------------------------------------------------------------------
// backend_sources (phase 8)
// -----------------------------------------------------------------------

#[test]
fn static_source_folds_into_pool_targets() {
    let yaml = r#"
backend_sources:
  - name: eu
    type: static
    targets: ["10.1.0.1:7777", "10.1.0.2:7777"]
pools:
  - { name: p, source: eu }
listeners:
  - { name: l, bind: "0.0.0.0:7777", pool: p }
"#;
    let cfg = parse_str(yaml).unwrap();
    assert_eq!(
        cfg.pools[0].targets,
        vec![
            "10.1.0.1:7777".parse().unwrap(),
            "10.1.0.2:7777".parse().unwrap()
        ]
    );
    assert!(cfg.pools[0].source.is_none());
}

#[test]
fn dynamic_source_attaches_to_the_pool_with_an_empty_seed() {
    let yaml = r#"
backend_sources:
  - name: us
    type: dns_srv
    record: "_game._udp.us.example.com"
    refresh_interval_sec: 10
pools:
  - { name: p, source: us }
listeners:
  - { name: l, bind: "0.0.0.0:7777", pool: p }
"#;
    let cfg = parse_str(yaml).unwrap();
    assert!(cfg.pools[0].targets.is_empty());
    let sc = cfg.pools[0].source.as_ref().unwrap();
    assert_eq!(sc.name, "us");
    assert_eq!(sc.refresh_interval, Duration::from_secs(10));
    assert!(matches!(
        &sc.kind,
        SourceKind::DnsSrv { record } if record == "_game._udp.us.example.com"
    ));
}

#[test]
fn consul_and_kubernetes_sources_apply_defaults() {
    let yaml = r#"
backend_sources:
  - { name: c, type: consul, service: game }
  - { name: k, type: kubernetes, service: match }
pools:
  - { name: pc, source: c }
  - { name: pk, source: k }
listeners:
  - { name: l, bind: "0.0.0.0:7777", pool: pc }
"#;
    let cfg = parse_str(yaml).unwrap();
    let by = |n: &str| {
        cfg.pools
            .iter()
            .find(|p| p.name == n)
            .unwrap()
            .source
            .clone()
            .unwrap()
    };
    assert!(matches!(
        by("pc").kind,
        SourceKind::Consul { addr, tag: None, .. } if addr == "http://127.0.0.1:8500"
    ));
    assert!(matches!(
        by("pk").kind,
        SourceKind::Kubernetes { namespace, api, port_name: None, .. }
            if namespace == "default" && api == "https://kubernetes.default.svc"
    ));
}

#[test]
fn rejects_targets_and_source_together() {
    let yaml = r#"
backend_sources: [{ name: s, type: static, targets: ["10.0.0.1:1"] }]
pools:
  - { name: p, targets: ["127.0.0.1:1"], source: s }
listeners:
  - { name: l, bind: "0.0.0.0:7777", pool: p }
"#;
    assert!(parse_str(yaml).is_err());
}

#[test]
fn rejects_pool_with_neither_targets_nor_source() {
    let yaml = r#"
pools:
  - { name: p }
listeners:
  - { name: l, bind: "0.0.0.0:7777", pool: p }
"#;
    assert!(parse_str(yaml).is_err());
}

#[test]
fn rejects_unknown_source_reference_and_bad_source_specs() {
    // unknown reference
    assert!(parse_str(
        r#"
pools: [{ name: p, source: nope }]
listeners: [{ name: l, bind: "0.0.0.0:7777", pool: p }]
"#
    )
    .is_err());
    // dns_srv without `record`
    assert!(parse_str(
        r#"
backend_sources: [{ name: s, type: dns_srv }]
pools: [{ name: p, source: s }]
listeners: [{ name: l, bind: "0.0.0.0:7777", pool: p }]
"#
    )
    .is_err());
    // refresh_interval_sec: 0
    assert!(parse_str(
        r#"
backend_sources: [{ name: s, type: consul, service: g, refresh_interval_sec: 0 }]
pools: [{ name: p, source: s }]
listeners: [{ name: l, bind: "0.0.0.0:7777", pool: p }]
"#
    )
    .is_err());
    // unknown type
    assert!(parse_str(
        r#"
backend_sources: [{ name: s, type: etcd, service: g }]
pools: [{ name: p, source: s }]
listeners: [{ name: l, bind: "0.0.0.0:7777", pool: p }]
"#
    )
    .is_err());
    // tunnel without `pubkey`
    assert!(parse_str(
        r#"
backend_sources: [{ name: s, type: tunnel }]
pools: [{ name: p, source: s }]
listeners: [{ name: l, bind: "0.0.0.0:7777", pool: p }]
"#
    )
    .is_err());
    // tunnel with a malformed pubkey (not 32 bytes of base64)
    assert!(parse_str(
        r#"
backend_sources: [{ name: s, type: tunnel, pubkey: "not-a-key" }]
pools: [{ name: p, source: s }]
listeners: [{ name: l, bind: "0.0.0.0:7777", pool: p }]
"#
    )
    .is_err());
}

#[test]
fn tunnel_source_attaches_to_the_pool_with_the_pinned_pubkey() {
    let yaml = r#"
backend_sources:
  - name: home
    type: tunnel
    pubkey: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
    refresh_interval_sec: 10
pools:
  - { name: p, source: home }
listeners:
  - { name: l, bind: "0.0.0.0:7777", pool: p }
"#;
    let cfg = parse_str(yaml).unwrap();
    assert!(cfg.pools[0].targets.is_empty());
    let sc = cfg.pools[0].source.as_ref().unwrap();
    assert_eq!(sc.name, "home");
    assert!(matches!(
        &sc.kind,
        SourceKind::Tunnel { pubkey }
            if pubkey == "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
    ));
}

fn gossip_fixture(settings_extra: &str) -> String {
    format!(
        "settings:\n{settings_extra}\
         pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
         listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n"
    )
}

#[test]
fn absent_gossip_is_none() {
    let yaml = "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
                listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n";
    let cfg = parse_str(yaml).unwrap();
    assert!(cfg.failure_domain.is_none());
    assert!(cfg.gossip.is_none());
}

#[test]
fn parses_gossip_settings_with_defaults() {
    let yaml = gossip_fixture(
        "  failure_domain: \"eu-west-1a\"\n\
         \x20 gossip:\n    bind: \"0.0.0.0:7946\"\n    psk: \"secret\"\n",
    );
    let cfg = parse_str(&yaml).unwrap();
    assert_eq!(cfg.failure_domain.as_deref(), Some("eu-west-1a"));
    let g = cfg.gossip.unwrap();
    assert_eq!(g.bind, "0.0.0.0:7946".parse().unwrap());
    assert!(g.seeds.is_empty());
    assert_eq!(g.quorum_fraction, 0.66);
    assert_eq!(g.psk, "secret");
}

#[test]
fn parses_gossip_seeds_and_quorum_fraction() {
    let yaml = gossip_fixture(
        "  failure_domain: \"eu-west-1a\"\n\
         \x20 gossip:\n    bind: \"0.0.0.0:7946\"\n    psk: \"secret\"\n    \
         quorum_fraction: 0.75\n    seeds: [\"10.0.0.1:7946\", \"10.0.0.2:7946\"]\n",
    );
    let g = parse_str(&yaml).unwrap().gossip.unwrap();
    assert_eq!(g.quorum_fraction, 0.75);
    assert_eq!(
        g.seeds,
        vec![
            "10.0.0.1:7946".parse().unwrap(),
            "10.0.0.2:7946".parse().unwrap(),
        ]
    );
}

#[test]
fn rejects_gossip_without_failure_domain_and_vice_versa() {
    let gossip_only =
        gossip_fixture("  gossip:\n    bind: \"0.0.0.0:7946\"\n    psk: \"secret\"\n");
    assert!(parse_str(&gossip_only).is_err());

    let domain_only = gossip_fixture("  failure_domain: \"eu-west-1a\"\n");
    assert!(parse_str(&domain_only).is_err());
}

#[test]
fn rejects_bad_gossip_settings() {
    for bad in [
        // bad bind address
        gossip_fixture(
            "  failure_domain: \"d\"\n  gossip:\n    bind: \"nope\"\n    psk: \"secret\"\n",
        ),
        // bad seed address
        gossip_fixture(
            "  failure_domain: \"d\"\n  gossip:\n    bind: \"0.0.0.0:7946\"\n    \
             psk: \"secret\"\n    seeds: [\"nope\"]\n",
        ),
        // empty psk
        gossip_fixture(
            "  failure_domain: \"d\"\n  gossip:\n    bind: \"0.0.0.0:7946\"\n    psk: \"\"\n",
        ),
        // quorum_fraction too low
        gossip_fixture(
            "  failure_domain: \"d\"\n  gossip:\n    bind: \"0.0.0.0:7946\"\n    \
             psk: \"secret\"\n    quorum_fraction: 0.5\n",
        ),
        // quorum_fraction too high
        gossip_fixture(
            "  failure_domain: \"d\"\n  gossip:\n    bind: \"0.0.0.0:7946\"\n    \
             psk: \"secret\"\n    quorum_fraction: 1.5\n",
        ),
    ] {
        assert!(parse_str(&bad).is_err(), "should reject: {bad}");
    }
}

#[test]
fn absent_group_is_none() {
    let yaml = "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
                listeners:\n  - name: l\n    bind: \"0.0.0.0:7777\"\n    pool: p\n";
    assert!(parse_str(yaml).unwrap().group.is_none());
}

#[test]
fn parses_valid_multi_segment_group() {
    let yaml = gossip_fixture("  group: \"eu/frankfurt/cluster-a\"\n");
    assert_eq!(
        parse_str(&yaml).unwrap().group.as_deref(),
        Some("eu/frankfurt/cluster-a")
    );
}

#[test]
fn rejects_bad_group_paths() {
    for bad in [
        gossip_fixture("  group: \"\"\n"),
        gossip_fixture("  group: \"/eu/frankfurt\"\n"),
        gossip_fixture("  group: \"eu/frankfurt/\"\n"),
        gossip_fixture("  group: \"eu//frankfurt\"\n"),
    ] {
        assert!(parse_str(&bad).is_err(), "should reject: {bad}");
    }
}
