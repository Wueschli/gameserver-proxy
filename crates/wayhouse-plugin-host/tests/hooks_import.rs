//! The `on_webhook` and `on_event` exports and the `webhook_respond` import.

use std::collections::BTreeMap;

use wayhouse_plugin_host::{inspect, CallError, Limits, PluginHost, StateSnapshot, WebhookRequest};

const CAPS: &str = r#"{"triggers":{"on_timer":true,"on_webhook":true,"on_event":["config_revision"]},"tick_interval_secs":30,"log":true}"#;
const RESPONSE: &str = r#"{"status":202,"headers":{"X-Run":"1"},"body":"b2s="}"#;

/// `on_webhook` logs the request document it was given and answers with RESPONSE;
/// `on_event` logs `kind` then `payload`.
fn guest(caps: &str) -> Vec<u8> {
    let caps = caps.replace('"', "\\\"");
    let resp = RESPONSE.replace('"', "\\\"");
    let rlen = RESPONSE.len();
    wat::parse_str(format!(
        r#"(module
  (@custom "wayhouse.plugin-abi" "\00\00\01\00")
  (@custom "wayhouse.plugin-caps" "{caps}")
  (import "wayhouse" "log" (func $log (param i32 i32 i32)))
  (import "wayhouse" "webhook_respond" (func $respond (param i32 i32) (result i32)))
  (memory (export "memory") 2)
  (data (i32.const 60000) "{resp}")
  (global $bump (mut i32) (i32.const 1024))
  (func (export "alloc") (param i32) (result i32) (local $p i32)
    (local.set $p (global.get $bump))
    (global.set $bump (i32.add (global.get $bump) (local.get 0)))
    (local.get $p))
  (func (export "init") (param i32 i32))
  (func (export "on_timer"))
  (func (export "on_webhook") (param $p i32) (param $n i32)
    (call $log (i32.const 2) (local.get $p) (local.get $n))
    (drop (call $respond (i32.const 60000) (i32.const {rlen}))))
  (func (export "on_event") (param $kp i32) (param $kn i32) (param $pp i32) (param $pn i32)
    (call $log (i32.const 2) (local.get $kp) (local.get $kn))
    (call $log (i32.const 2) (local.get $pp) (local.get $pn))))"#
    ))
    .unwrap()
}

fn plugin(caps: &str) -> wayhouse_plugin_host::Plugin {
    let m = guest(caps);
    let approved = inspect(&m).unwrap().caps;
    PluginHost::new(Limits::default())
        .unwrap()
        .load(&m, &approved)
        .unwrap()
}

#[test]
fn a_webhook_call_gets_the_request_and_returns_the_guests_answer() {
    let req = WebhookRequest {
        method: "POST".into(),
        suffix: "/server-created".into(),
        query: "a=1".into(),
        headers: BTreeMap::from([("content-type".to_string(), "application/json".to_string())]),
        body: b"{\"id\":7}".to_vec(),
        idempotency_key: "key-1".into(),
    };
    let fx = plugin(CAPS)
        .on_webhook(&StateSnapshot::new(), &req)
        .unwrap();
    let seen = &fx.logs[0].msg;
    assert!(
        seen.contains("/server-created") && seen.contains("key-1"),
        "{seen}"
    );
    // The body arrives base64.
    assert!(seen.contains("eyJpZCI6N30="), "{seen}");
    let r = fx.webhook.unwrap();
    assert_eq!((r.status, r.body.as_slice()), (202, &b"ok"[..]));
    assert_eq!(r.headers["x-run"], "1");
}

#[test]
fn an_event_call_gets_the_kind_and_payload() {
    let fx = plugin(CAPS)
        .on_event(
            &StateSnapshot::new(),
            "config_revision",
            br#"{"revision":9}"#,
        )
        .unwrap();
    assert_eq!(fx.logs[0].msg, "config_revision");
    assert_eq!(fx.logs[1].msg, r#"{"revision":9}"#);
}

#[test]
fn an_undeclared_trigger_is_never_delivered() {
    let p = plugin(CAPS);
    assert!(matches!(
        p.on_event(&StateSnapshot::new(), "plugin_changed", b"{}"),
        Err(CallError::CapabilityDenied("on_event"))
    ));
    let timer_only = r#"{"triggers":{"on_timer":true},"tick_interval_secs":30,"log":true}"#;
    let m = wat::parse_str(format!(
        r#"(module
  (@custom "wayhouse.plugin-abi" "\00\00\01\00")
  (@custom "wayhouse.plugin-caps" "{}")
  (memory (export "memory") 1)
  (func (export "alloc") (param i32) (result i32) (i32.const 1024))
  (func (export "init") (param i32 i32))
  (func (export "on_timer")))"#,
        timer_only.replace('"', "\\\"")
    ))
    .unwrap();
    let approved = inspect(&m).unwrap().caps;
    let p = PluginHost::new(Limits::default())
        .unwrap()
        .load(&m, &approved)
        .unwrap();
    assert!(matches!(
        p.on_webhook(&StateSnapshot::new(), &WebhookRequest::default()),
        Err(CallError::CapabilityDenied("on_webhook"))
    ));
}

#[test]
fn on_event_needs_on_timer_and_known_events() {
    for caps in [
        r#"{"triggers":{"on_event":["config_revision"]},"log":true}"#,
        r#"{"triggers":{"on_timer":true,"on_event":["nope"]},"tick_interval_secs":30}"#,
        r#"{"triggers":{"on_timer":true,"on_event":["config_revision","config_revision"]},"tick_interval_secs":30}"#,
    ] {
        assert!(
            wayhouse_plugin_host::Capabilities::parse(caps.as_bytes()).is_err(),
            "{caps}"
        );
    }
}

#[test]
fn a_module_cannot_declare_a_trigger_the_operator_did_not_approve() {
    let m = guest(CAPS);
    let declared = inspect(&m).unwrap().caps;
    let mut approved = declared.clone();
    approved.triggers.on_webhook = false;
    assert!(PluginHost::new(Limits::default())
        .unwrap()
        .load(&m, &approved)
        .is_err());
    let mut approved = declared;
    approved.triggers.on_event.clear();
    assert!(PluginHost::new(Limits::default())
        .unwrap()
        .load(&m, &approved)
        .is_err());
}
