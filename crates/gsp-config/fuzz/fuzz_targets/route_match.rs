#![no_main]
//! Routing a connection with a fuzzed first-bytes buffer against a fixed set of
//! matchers (`first_bytes`, `sni`, `client_cidr`, `port`). Exercises
//! `Matcher::matches` / `extract_sni` / `HostPattern` on untrusted peek bytes.

use std::net::SocketAddr;
use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;

fn listener() -> &'static gsp_config::ListenerConfig {
    static CFG: OnceLock<gsp_config::Config> = OnceLock::new();
    &CFG.get_or_init(|| {
        gsp_config::parse_str(
            r#"
pools:
  - { name: p, targets: ["127.0.0.1:1"] }
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    routes:
      - { match: { type: first_bytes, prefix: "hex:ffffffff" }, action: { pool: p } }
      - { match: { type: first_bytes, length: { min: 0, max: 32 } }, action: { pool: p } }
      - { match: { type: sni, host: ["*.example.com", "eu.example.net"] }, action: { pool: p } }
      - { match: { type: always }, action: { pool: p } }
"#,
        )
        .expect("static fuzz config parses")
    })
    .listeners[0]
}

fuzz_target!(|data: &[u8]| {
    let l = listener();
    let ctx = gsp_config::MatchContext {
        src: "203.0.113.7:40000".parse::<SocketAddr>().unwrap(),
        local: "198.51.100.1:7777".parse::<SocketAddr>().unwrap(),
        first_bytes: data,
        sniff: None,
    };
    let _ = l.route_for(&ctx);
    let _ = l.first_packet_recognised(&ctx);
});
