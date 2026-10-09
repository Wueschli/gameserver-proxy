//! The `routes_set` import against a WAT guest.

use wayhouse_plugin_host::{inspect, CallError, Limits, PluginHost, RouteEntry, StateSnapshot};

const CAPS: &str = r#"{"triggers":{"on_timer":true},"tick_interval_secs":30,"log":true,"routes":{"hosts":["*.mc.example.com"],"backends":["10.0.0.0/16"],"max_entries":4}}"#;

/// Declares `doc` as the route set and logs the import's return value as a digit
/// (`0` accepted, `1` refused).
fn guest(caps: &str, doc: &str) -> Vec<u8> {
    let caps = caps.replace('"', "\\\"");
    let data = doc.replace('"', "\\\"");
    let len = doc.len();
    wat::parse_str(format!(
        r#"(module
  (@custom "wayhouse.plugin-abi" "\00\00\01\00")
  (@custom "wayhouse.plugin-caps" "{caps}")
  (import "wayhouse" "log" (func $log (param i32 i32 i32)))
  (import "wayhouse" "routes_set" (func $set (param i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "{data}")
  (data (i32.const 2000) "01")
  (func (export "alloc") (param i32) (result i32) (i32.const 4096))
  (func (export "init") (param i32 i32))
  (func (export "on_timer")
    (call $log (i32.const 2) (i32.add (i32.const 2000)
      (i32.lt_s (call $set (i32.const 0) (i32.const {len})) (i32.const 0))) (i32.const 1))))"#
    ))
    .unwrap()
}

fn run(caps: &str, doc: &str) -> Result<wayhouse_plugin_host::Effects, CallError> {
    let m = guest(caps, doc);
    let approved = inspect(&m).unwrap().caps;
    PluginHost::new(Limits::default())
        .unwrap()
        .load(&m, &approved)
        .unwrap()
        .on_timer(&StateSnapshot::new())
}

#[test]
fn an_approved_set_is_returned_sorted() {
    let fx = run(
        CAPS,
        r#"[{"host":"b.mc.example.com","backend":"10.0.1.2:25565"},{"host":"a.mc.example.com","backend":"10.0.1.3:25565"}]"#,
    )
    .unwrap();
    assert_eq!(fx.logs[0].msg, "0");
    let routes = fx.routes.unwrap();
    assert_eq!(routes.len(), 2);
    assert_eq!(
        routes[0],
        RouteEntry {
            host: "a.mc.example.com".into(),
            backend: "10.0.1.3:25565".into()
        }
    );
}

#[test]
fn an_empty_set_clears_the_routes() {
    let fx = run(CAPS, "[]").unwrap();
    assert_eq!(fx.routes, Some(vec![]));
}

#[test]
fn a_refused_set_changes_nothing_and_says_why() {
    for doc in [
        r#"[{"host":"other.example.com","backend":"10.0.1.2:25565"}]"#,
        r#"[{"host":"a.mc.example.com","backend":"192.168.1.2:25565"}]"#,
        r#"[{"host":"a.mc.example.com","backend":"10.0.1.2:25565","x":1}]"#,
        "not json",
    ] {
        let fx = run(CAPS, doc).unwrap();
        assert!(fx.routes.is_none(), "{doc}");
        assert_eq!(fx.logs.last().unwrap().msg, "1", "{doc}");
        assert!(
            fx.logs
                .iter()
                .any(|l| l.msg.starts_with("routes_set refused")),
            "{doc}"
        );
    }
}

#[test]
fn routes_without_the_capability_trap_the_call() {
    let caps = r#"{"triggers":{"on_timer":true},"tick_interval_secs":30,"log":true}"#;
    assert!(matches!(
        run(caps, "[]"),
        Err(CallError::CapabilityDenied("routes"))
    ));
}

#[test]
fn a_module_cannot_declare_more_routes_than_were_approved() {
    let m = guest(CAPS, "[]");
    let declared = inspect(&m).unwrap().caps;
    let mut approved = declared.clone();
    approved.routes.as_mut().unwrap().max_entries = 2;
    assert!(PluginHost::new(Limits::default())
        .unwrap()
        .load(&m, &approved)
        .is_err());
}
