//! Hostile-module tests: each must be rejected before compilation, naming the bound.

use wayhouse_plugin_host::{Limits, ModuleError, PluginHost};

const ABI: &str = "\\00\\00\\01\\00";
const CAPS: &str = r#"{\"log\":true}"#;

fn module(body: &str) -> Vec<u8> {
    let src = format!(
        r#"(module
  (@custom "wayhouse.plugin-abi" "{ABI}")
  (@custom "wayhouse.plugin-caps" "{CAPS}")
  (memory (export "memory") 1)
  (func (export "alloc") (param i32) (result i32) (i32.const 0))
  (func (export "init") (param i32 i32))
  {body})"#
    );
    wat::parse_str(src).unwrap()
}

fn rejected(body: &str, expect: &str) {
    let m = module(body);
    let host = PluginHost::new(Limits::default()).unwrap();
    let approved = wayhouse_plugin_host::inspect(&m).unwrap().caps;
    match host.load(&m, &approved) {
        Err(ModuleError::TooComplex(msg)) => assert!(msg.contains(expect), "{msg}"),
        other => panic!("expected TooComplex({expect}), got {:?}", other.err()),
    }
}

#[test]
fn a_normal_module_passes() {
    let m = module("");
    let host = PluginHost::new(Limits::default()).unwrap();
    let approved = wayhouse_plugin_host::inspect(&m).unwrap().caps;
    host.load(&m, &approved).unwrap();
}

#[test]
fn too_many_functions() {
    rejected(&"(func)".repeat(100_000), "functions");
}

#[test]
fn too_many_locals_in_one_function() {
    rejected(
        &format!("(func (local {}))", "i32 ".repeat(100_000)),
        "locals",
    );
}

#[test]
fn nesting_too_deep() {
    let body = format!(
        "(func {} {})",
        "block ".repeat(10_000),
        "end ".repeat(10_000)
    );
    rejected(&body, "nesting");
}

#[test]
fn table_too_large() {
    rejected("(table 100000000 funcref)", "table");
}

#[test]
fn memory_too_large() {
    rejected("(memory 65536)", "memory");
}

#[test]
fn too_many_globals() {
    rejected(&"(global i32 (i32.const 0))".repeat(10_000), "globals");
}
