//! A fixed set of modules and manifests must produce a byte-identical index, so the
//! sniffers repo CI diffs of `index.json` only ever show real changes.
//! Regenerate with `UPDATE_GOLDEN=1 cargo test -p wayhouse-registry --test generate_golden`
//! and review the diff by hand.

use wayhouse_registry::{generate, parse_manifest, to_json, Artifact};

fn artifact(name: &str, version: &str, marker: &str) -> Artifact {
    let manifest = parse_manifest(&format!(
        r#"name = "{name}"
description = "{name} sniffer"
license = "MIT OR Apache-2.0"
version = "{version}"
abi = "0.1"
min_proxy = "0.1.0"
[limits]
max_memory_bytes = 1048576
call_timeout_ms = 50
"#
    ))
    .unwrap();
    let bytes = wat::parse_str(format!(
        r#"(module (memory 1) (func (export "{marker}")) (@custom "wayhouse.abi" "\00\00\01\00"))"#
    ))
    .unwrap();
    Artifact {
        manifest,
        bytes,
        url: format!("https://example.com/dl/{name}-v{version}/{name}.wasm"),
        signature_url: Some(format!(
            "https://example.com/dl/{name}-v{version}/{name}.wasm.minisig"
        )),
    }
}

#[test]
fn generated_index_matches_the_golden_file() {
    let g = generate(
        "golden registry",
        &[
            artifact("wireguard", "0.1.0", "w"),
            artifact("a2s", "0.2.0", "b"),
            artifact("a2s", "0.1.0", "a"),
        ],
        None,
    )
    .unwrap();
    let got = to_json(&g.index);
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/index.json");
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(std::path::Path::new(path).parent().unwrap()).unwrap();
        std::fs::write(path, &got).unwrap();
    }
    let want = std::fs::read_to_string(path).expect("golden file; run with UPDATE_GOLDEN=1 once");
    assert_eq!(got, want);
}
