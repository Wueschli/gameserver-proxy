//! Runtime behaviour against hand-written WAT guests.

use std::time::Duration;

use wayhouse_plugin_host::{CallError, Limits, LogLevel, ModuleError, PluginHost, StateSnapshot};

const ABI: &str = "\\00\\00\\01\\00";
const TIMER_LOG_STATE: &str =
    r#"{"triggers":{"on_timer":true},"tick_interval_secs":30,"log":true,"state":{"max_bytes":64}}"#;

/// A guest with the standard imports, memory, a bump `alloc`, and the data segments
/// `"hello"@0`, `"k"@16`, `"v1"@32`, `"r"@48`, `"abc"@64`.
fn guest(caps: &str, funcs: &str) -> Vec<u8> {
    let caps = caps.replace('\\', "\\\\").replace('"', "\\\"");
    let src = format!(
        r#"(module
  (@custom "wayhouse.plugin-abi" "{ABI}")
  (@custom "wayhouse.plugin-caps" "{caps}")
  (import "wayhouse" "log" (func $log (param i32 i32 i32)))
  (import "wayhouse" "state_get" (func $get (param i32 i32 i32 i32) (result i32)))
  (import "wayhouse" "state_put" (func $put (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "hello")
  (data (i32.const 16) "k")
  (data (i32.const 32) "v1")
  (data (i32.const 48) "r")
  (data (i32.const 64) "abc")
  (global $bump (mut i32) (i32.const 1024))
  (func (export "alloc") (param i32) (result i32)
    (local $p i32)
    (local.set $p (global.get $bump))
    (global.set $bump (i32.add (global.get $bump) (local.get 0)))
    (local.get $p))
  {funcs})"#
    );
    wat::parse_str(src).unwrap()
}

fn host() -> PluginHost {
    PluginHost::new(Limits::default()).unwrap()
}

#[test]
fn on_timer_logs_and_writes_state() {
    let m = guest(
        TIMER_LOG_STATE,
        r#"(func (export "init") (param i32 i32))
           (func (export "on_timer")
             (call $log (i32.const 2) (i32.const 0) (i32.const 5))
             (drop (call $put (i32.const 16) (i32.const 1) (i32.const 32) (i32.const 2))))"#,
    );
    let plugin = host().load(&m).unwrap();
    let fx = plugin.on_timer(&StateSnapshot::new()).unwrap();
    assert_eq!(fx.logs.len(), 1);
    assert_eq!(fx.logs[0].level, LogLevel::Info);
    assert_eq!(fx.logs[0].msg, "hello");
    assert_eq!(fx.state_puts["k"], b"v1");
}

#[test]
fn init_receives_its_config() {
    let m = guest(
        TIMER_LOG_STATE,
        r#"(func (export "init") (param i32 i32)
             (drop (call $put (i32.const 16) (i32.const 1) (local.get 0) (local.get 1))))
           (func (export "on_timer"))"#,
    );
    let plugin = host().load(&m).unwrap();
    let fx = plugin.init(b"{\"a\":1}", &StateSnapshot::new()).unwrap();
    assert_eq!(fx.state_puts["k"], b"{\"a\":1}");
}

const READ_K_INTO_R: &str = r#"
    (func (export "init") (param i32 i32))
    (func (export "on_timer")
      (local $n i32)
      PRE
      (local.set $n (call $get (i32.const 16) (i32.const 1) (i32.const 100) (i32.const 16)))
      (drop (call $put (i32.const 48) (i32.const 1) (i32.const 100) (local.get $n))))"#;

#[test]
fn state_get_sees_the_snapshot() {
    let m = guest(TIMER_LOG_STATE, &READ_K_INTO_R.replace("PRE", ""));
    let plugin = host().load(&m).unwrap();
    let mut snap = StateSnapshot::new();
    snap.insert("k".into(), b"abc".to_vec());
    let fx = plugin.on_timer(&snap).unwrap();
    assert_eq!(fx.state_puts["r"], b"abc");
}

#[test]
fn state_get_sees_the_calls_own_writes_first() {
    let pre = "(drop (call $put (i32.const 16) (i32.const 1) (i32.const 32) (i32.const 2)))";
    let m = guest(TIMER_LOG_STATE, &READ_K_INTO_R.replace("PRE", pre));
    let plugin = host().load(&m).unwrap();
    let mut snap = StateSnapshot::new();
    snap.insert("k".into(), b"abc".to_vec());
    let fx = plugin.on_timer(&snap).unwrap();
    assert_eq!(fx.state_puts["r"], b"v1");
}

#[test]
fn state_get_with_a_small_buffer_reports_the_length_and_writes_nothing() {
    // k = "abc" is 3 bytes; a 1-byte buffer must return 3, and the byte stays 0.
    let m = guest(
        TIMER_LOG_STATE,
        r#"(func (export "init") (param i32 i32))
           (func (export "on_timer")
             (if (i32.ne (call $get (i32.const 16) (i32.const 1) (i32.const 100) (i32.const 1))
                         (i32.const 3))
               (then unreachable))
             (if (i32.ne (i32.load8_u (i32.const 100)) (i32.const 0)) (then unreachable)))"#,
    );
    let plugin = host().load(&m).unwrap();
    let mut snap = StateSnapshot::new();
    snap.insert("k".into(), b"abc".to_vec());
    plugin.on_timer(&snap).unwrap();
}

#[test]
fn state_put_over_the_cap_returns_minus_one() {
    // cap is 64 bytes; a 100-byte value from memory must be refused with -1.
    let m = guest(
        TIMER_LOG_STATE,
        r#"(func (export "init") (param i32 i32))
           (func (export "on_timer")
             (if (i32.ne (call $put (i32.const 16) (i32.const 1) (i32.const 0) (i32.const 100))
                         (i32.const -1))
               (then unreachable)))"#,
    );
    let plugin = host().load(&m).unwrap();
    let fx = plugin.on_timer(&StateSnapshot::new()).unwrap();
    assert!(fx.state_puts.is_empty());
}

#[test]
fn state_without_the_capability_is_denied() {
    let m = guest(
        r#"{"triggers":{"on_timer":true},"tick_interval_secs":30,"log":true}"#,
        r#"(func (export "init") (param i32 i32))
           (func (export "on_timer")
             (drop (call $put (i32.const 16) (i32.const 1) (i32.const 32) (i32.const 2))))"#,
    );
    let plugin = host().load(&m).unwrap();
    assert!(matches!(
        plugin.on_timer(&StateSnapshot::new()),
        Err(CallError::CapabilityDenied("state"))
    ));
}

#[test]
fn log_without_the_capability_is_denied() {
    let m = guest(
        r#"{"triggers":{"on_timer":true},"tick_interval_secs":30}"#,
        r#"(func (export "init") (param i32 i32))
           (func (export "on_timer") (call $log (i32.const 2) (i32.const 0) (i32.const 5)))"#,
    );
    let plugin = host().load(&m).unwrap();
    assert!(matches!(
        plugin.on_timer(&StateSnapshot::new()),
        Err(CallError::CapabilityDenied("log"))
    ));
}

#[test]
fn an_out_of_bounds_pointer_traps_the_call_and_the_host_survives() {
    let m = guest(
        TIMER_LOG_STATE,
        r#"(func (export "init") (param i32 i32))
           (func (export "on_timer") (call $log (i32.const 2) (i32.const 70000) (i32.const 10)))"#,
    );
    let plugin = host().load(&m).unwrap();
    assert!(matches!(
        plugin.on_timer(&StateSnapshot::new()),
        Err(CallError::Trap(_))
    ));
    // The plugin is still usable for the next call.
    assert!(matches!(
        plugin.on_timer(&StateSnapshot::new()),
        Err(CallError::Trap(_))
    ));
}

#[test]
fn an_endless_loop_times_out() {
    let m = guest(
        TIMER_LOG_STATE,
        r#"(func (export "init") (param i32 i32))
           (func (export "on_timer") (loop $l (br $l)))"#,
    );
    let host = PluginHost::new(Limits {
        call_timeout: Duration::from_millis(100),
        ..Limits::default()
    })
    .unwrap();
    let plugin = host.load(&m).unwrap();
    assert!(matches!(
        plugin.on_timer(&StateSnapshot::new()),
        Err(CallError::Timeout)
    ));
}

#[test]
fn memory_growth_past_the_cap_traps() {
    let m = guest(
        TIMER_LOG_STATE,
        r#"(func (export "init") (param i32 i32))
           (func (export "on_timer") (drop (memory.grow (i32.const 100))))"#,
    );
    let host = PluginHost::new(Limits {
        max_memory_bytes: 2 * 65536,
        ..Limits::default()
    })
    .unwrap();
    let plugin = host.load(&m).unwrap();
    assert!(matches!(
        plugin.on_timer(&StateSnapshot::new()),
        Err(CallError::Trap(_))
    ));
}

#[test]
fn log_lines_beyond_the_limit_are_dropped_and_counted() {
    let one = "(call $log (i32.const 2) (i32.const 0) (i32.const 5))";
    let m = guest(
        TIMER_LOG_STATE,
        &format!(
            r#"(func (export "init") (param i32 i32))
               (func (export "on_timer") {one} {one} {one} {one} {one})"#
        ),
    );
    let host = PluginHost::new(Limits {
        max_log_lines: 2,
        ..Limits::default()
    })
    .unwrap();
    let fx = host
        .load(&m)
        .unwrap()
        .on_timer(&StateSnapshot::new())
        .unwrap();
    assert_eq!(fx.logs.len(), 2);
    assert_eq!(fx.logs_dropped, 3);
}

#[test]
fn an_import_outside_the_abi_is_rejected_at_load() {
    let caps = r#"{\"log\":true}"#;
    let src = format!(
        r#"(module
  (@custom "wayhouse.plugin-abi" "{ABI}")
  (@custom "wayhouse.plugin-caps" "{caps}")
  (import "wasi_snapshot_preview1" "fd_write" (func (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (func (export "alloc") (param i32) (result i32) (i32.const 0))
  (func (export "init") (param i32 i32)))"#
    );
    let m = wat::parse_str(src).unwrap();
    assert!(matches!(
        host().load(&m),
        Err(ModuleError::UnexpectedImport(_))
    ));
}

#[test]
fn a_declared_timer_without_the_export_is_rejected_at_load() {
    let m = guest(TIMER_LOG_STATE, r#"(func (export "init") (param i32 i32))"#);
    assert!(matches!(
        host().load(&m),
        Err(ModuleError::MissingExport("on_timer"))
    ));
}

#[test]
fn an_undeclared_timer_is_never_called() {
    let m = guest(
        r#"{"log":true}"#,
        r#"(func (export "init") (param i32 i32))
           (func (export "on_timer") (call $log (i32.const 2) (i32.const 0) (i32.const 5)))"#,
    );
    let plugin = host().load(&m).unwrap();
    assert!(matches!(
        plugin.on_timer(&StateSnapshot::new()),
        Err(CallError::CapabilityDenied("on_timer"))
    ));
}
