//! The `http_request` / `http_read` imports against a WAT guest and a fake transport.

use base64::Engine;
use std::sync::{Arc, Mutex};

use wayhouse_plugin_host::http::{Hop, WireResponse};
use wayhouse_plugin_host::{
    inspect, CallError, HttpEngine, Limits, PluginHost, SecretSource, SecretValue, StateSnapshot,
    Transport,
};

const REQUEST: &str = r#"{"method":"GET","url":"https://panel.example/x","headers":{"Authorization":"Bearer ${secret:PANEL_TOKEN}"}}"#;
const CAPS: &str = r#"{"triggers":{"on_timer":true},"tick_interval_secs":30,"log":true,
    "http":{"hosts":[{"host":"panel.example"}]},
    "secrets":[{"name":"PANEL_TOKEN","hosts":["panel.example"]}]}"#;

/// Sends REQUEST, reads the response document, and logs it.
fn guest(caps: &str, request: &str) -> Vec<u8> {
    let caps = caps.replace('"', "\\\"").replace('\n', " ");
    let req = request.replace('"', "\\\"").replace('$', "\\24");
    let len = request.len();
    wat::parse_str(format!(
        r#"(module
  (@custom "wayhouse.plugin-abi" "\00\00\01\00")
  (@custom "wayhouse.plugin-caps" "{caps}")
  (import "wayhouse" "log" (func $log (param i32 i32 i32)))
  (import "wayhouse" "http_request" (func $req (param i32 i32) (result i32)))
  (import "wayhouse" "http_read" (func $read (param i32 i32) (result i32)))
  (memory (export "memory") 2)
  (data (i32.const 0) "{req}")
  (func (export "alloc") (param i32) (result i32) (i32.const 4096))
  (func (export "init") (param i32 i32))
  (func (export "on_timer") (local $n i32)
    (local.set $n (call $req (i32.const 0) (i32.const {len})))
    ;; too small a buffer keeps the document; the second read gets it
    (drop (call $read (i32.const 8192) (i32.const 2)))
    (drop (call $read (i32.const 8192) (local.get $n)))
    (call $log (i32.const 2) (i32.const 8192) (local.get $n))))"#
    ))
    .unwrap()
}

#[derive(Default)]
struct Echo(Mutex<Vec<String>>);

impl Transport for Echo {
    fn send(&self, hop: &Hop) -> Result<WireResponse, String> {
        let auth = hop
            .headers
            .iter()
            .find(|(n, _)| n == "Authorization")
            .map(|(_, v)| v.to_string())
            .unwrap_or_default();
        self.0.lock().unwrap().push(auth.clone());
        Ok(WireResponse {
            status: 200,
            headers: vec![],
            body: auth.into_bytes(),
        })
    }
}

struct Token;

impl SecretSource for Token {
    fn get(&self, slot: &str) -> Result<Option<SecretValue>, String> {
        Ok((slot == "PANEL_TOKEN").then(|| SecretValue::new(b"s3cr3t-token-value".to_vec())))
    }
}

fn engine(m: &[u8], transport: Arc<Echo>) -> Arc<HttpEngine> {
    let caps = inspect(m).unwrap().caps;
    Arc::new(HttpEngine::new(
        caps.http.clone().unwrap(),
        caps.secrets.clone(),
        Arc::new(Token),
        transport,
    ))
}

#[test]
fn a_guest_calls_http_and_neither_it_nor_its_logs_ever_see_the_secret() {
    let m = guest(CAPS, REQUEST);
    let caps = inspect(&m).unwrap().caps;
    let transport = Arc::new(Echo::default());
    let plugin = PluginHost::new(Limits::default())
        .unwrap()
        .load(&m, &caps)
        .unwrap()
        .with_http(engine(&m, transport.clone()));
    let fx = plugin.on_timer(&StateSnapshot::new()).unwrap();
    assert!(fx.used_http);
    // The destination got the real token, the guest's view of the echo is redacted.
    assert_eq!(transport.0.lock().unwrap()[0], "Bearer s3cr3t-token-value");
    let logged = &fx.logs[0].msg;
    assert!(logged.contains("\"status\":200"), "{logged}");
    assert!(!logged.contains("s3cr3t"), "{logged}");
    let body = logged
        .split("\"body\":\"")
        .nth(1)
        .and_then(|b| b.split('"').next())
        .unwrap();
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(body)
        .unwrap();
    assert_eq!(decoded, b"Bearer [redacted]");
}

#[test]
fn http_without_the_capability_traps_the_call() {
    // The module declares no http, so the imports are denied.
    let m = guest(
        r#"{"triggers":{"on_timer":true},"tick_interval_secs":30,"log":true}"#,
        REQUEST,
    );
    let caps = inspect(&m).unwrap().caps;
    let plugin = PluginHost::new(Limits::default())
        .unwrap()
        .load(&m, &caps)
        .unwrap();
    assert!(matches!(
        plugin.on_timer(&StateSnapshot::new()),
        Err(CallError::CapabilityDenied("http"))
    ));
}

#[test]
fn without_an_engine_the_guest_is_told_there_is_no_network() {
    let m = guest(CAPS, REQUEST);
    let caps = inspect(&m).unwrap().caps;
    let plugin = PluginHost::new(Limits::default())
        .unwrap()
        .load(&m, &caps)
        .unwrap();
    let fx = plugin.on_timer(&StateSnapshot::new()).unwrap();
    assert!(fx.logs[0].msg.contains("no network"), "{}", fx.logs[0].msg);
}
