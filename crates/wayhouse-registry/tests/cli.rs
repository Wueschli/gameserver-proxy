//! The `wayhouse-registry-gen` binary, end to end.

use std::path::Path;
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_wayhouse-registry-gen");

fn manifest(name: &str, version: &str) -> String {
    format!(
        r#"name = "{name}"
description = "{name} sniffer"
license = "MIT"
version = "{version}"
abi = "0.1"
min_proxy = "0.1.0"
[limits]
max_memory_bytes = 1048576
call_timeout_ms = 50
"#
    )
}

/// `<root>/<name>/{manifest.toml, <name>.wasm}` with a module distinct per `marker`.
fn sniffer_dir(
    root: &Path,
    name: &str,
    version: &str,
    marker: &str,
    sig: bool,
) -> std::path::PathBuf {
    let dir = root.join(format!("{name}-{version}"));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("manifest.toml"), manifest(name, version)).unwrap();
    let wasm = wat::parse_str(format!(
        r#"(module (memory 1) (func (export "{marker}")) (@custom "wayhouse.abi" "\00\00\01\00"))"#
    ))
    .unwrap();
    std::fs::write(dir.join(format!("{name}.wasm")), wasm).unwrap();
    if sig {
        std::fs::write(dir.join(format!("{name}.wasm.minisig")), "sig").unwrap();
    }
    dir
}

fn run(args: &[&std::ffi::OsStr]) -> Output {
    Command::new(BIN).args(args).output().unwrap()
}

fn gen_args<'a>(
    out: &'a Path,
    previous: Option<&'a Path>,
    dirs: &'a [&'a Path],
) -> Vec<&'a std::ffi::OsStr> {
    let mut a: Vec<&std::ffi::OsStr> = vec![
        "generate".as_ref(),
        "--name".as_ref(),
        "test registry".as_ref(),
    ];
    a.extend([
        "--base-url".as_ref(),
        "https://example.com/dl".as_ref(),
        "--out".as_ref(),
        out.as_os_str(),
    ]);
    if let Some(p) = previous {
        a.extend(["--previous".as_ref(), p.as_os_str()]);
    }
    a.extend(dirs.iter().map(|d| d.as_os_str()));
    a
}

#[test]
fn generate_writes_a_parseable_index_and_prints_what_changed() {
    let t = tempfile::tempdir().unwrap();
    let d = sniffer_dir(t.path(), "a2s", "0.1.0", "x", true);
    let out = t.path().join("index.json");
    let r = run(&gen_args(&out, None, &[&d]));
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    assert!(String::from_utf8_lossy(&r.stdout).contains("added a2s 0.1.0"));
    let index = wayhouse_registry::parse_index(&std::fs::read(&out).unwrap()).unwrap();
    let v = &index.sniffers[0].versions[0];
    assert_eq!(v.url, "https://example.com/dl/a2s-v0.1.0/a2s.wasm");
    assert_eq!(
        v.signature_url.as_deref(),
        Some("https://example.com/dl/a2s-v0.1.0/a2s.wasm.minisig")
    );
}

#[test]
fn generate_with_previous_adds_a_version_and_can_overwrite_the_same_file() {
    let t = tempfile::tempdir().unwrap();
    let out = t.path().join("index.json");
    let d1 = sniffer_dir(t.path(), "a2s", "0.1.0", "x", false);
    assert!(run(&gen_args(&out, None, &[&d1])).status.success());
    let d2 = sniffer_dir(t.path(), "a2s", "0.2.0", "y", false);
    let r = run(&gen_args(&out, Some(&out), &[&d2]));
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    let index = wayhouse_registry::parse_index(&std::fs::read(&out).unwrap()).unwrap();
    assert_eq!(index.sniffers[0].versions.len(), 2);
}

#[test]
fn generate_refuses_to_change_a_published_version() {
    let t = tempfile::tempdir().unwrap();
    let out = t.path().join("index.json");
    let d1 = sniffer_dir(t.path(), "a2s", "0.1.0", "x", false);
    assert!(run(&gen_args(&out, None, &[&d1])).status.success());
    let before = std::fs::read(&out).unwrap();
    let rebuilt = t.path().join("rebuilt");
    let d2 = sniffer_dir(&rebuilt, "a2s", "0.1.0", "other", false);
    let r = run(&gen_args(&out, Some(&out), &[&d2]));
    assert!(!r.status.success());
    assert!(String::from_utf8_lossy(&r.stderr).contains("immutable"));
    assert_eq!(
        std::fs::read(&out).unwrap(),
        before,
        "a failed run must not touch the index"
    );
}

#[test]
fn verify_accepts_a_generated_index_and_rejects_garbage() {
    let t = tempfile::tempdir().unwrap();
    let out = t.path().join("index.json");
    let d = sniffer_dir(t.path(), "a2s", "0.1.0", "x", false);
    assert!(run(&gen_args(&out, None, &[&d])).status.success());
    assert!(run(&["verify".as_ref(), out.as_os_str()]).status.success());
    let bad = t.path().join("bad.json");
    std::fs::write(&bad, "{}").unwrap();
    let r = run(&["verify".as_ref(), bad.as_os_str()]);
    assert!(!r.status.success());
    assert!(!String::from_utf8_lossy(&r.stderr).is_empty());
}
