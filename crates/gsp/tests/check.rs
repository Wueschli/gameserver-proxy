//! `gsp --check` is the pre-deploy gate: it must refuse every setting this build
//! cannot run, not only the ones `main` happens to construct before it returns
//! (issue #127). Each test runs the real binary against a config that needs one
//! optional cargo feature and asserts on the build's feature set: a build
//! without the feature refuses with a message naming it, a build with it accepts.

use std::process::{Command, Output};

const BASE: &str = "pools:\n  - name: p\n    targets: [\"127.0.0.1:1\"]\n\
                    listeners:\n  - name: l\n    bind: \"127.0.0.1:0\"\n    pool: p\n";

fn check(yaml: &str, extra: &[&str]) -> Output {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gsp.yaml");
    std::fs::write(&path, yaml).unwrap();
    Command::new(env!("CARGO_BIN_EXE_gsp"))
        .arg("--check")
        .arg("--config")
        .arg(&path)
        .args(extra)
        .output()
        .unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn a_plain_config_passes() {
    let out = check(BASE, &[]);
    assert!(out.status.success(), "{}", stderr(&out));
}

#[test]
fn a_grpc_resolver_needs_the_grpc_resolver_feature() {
    let yaml = format!(
        "resolvers:\n  - name: mm\n    type: grpc\n    endpoint: \"http://127.0.0.1:1\"\n{BASE}"
    );
    let out = check(&yaml, &[]);
    if cfg!(feature = "grpc-resolver") {
        assert!(out.status.success(), "{}", stderr(&out));
    } else {
        assert!(!out.status.success());
        assert!(
            stderr(&out).contains("`grpc-resolver` cargo feature"),
            "{}",
            stderr(&out)
        );
    }
}

#[test]
fn a_dns_srv_source_needs_the_dns_srv_feature() {
    let yaml =
        "backend_sources:\n  - name: s\n    type: dns_srv\n    record: \"_g._udp.example.com\"\n\
                pools:\n  - name: p\n    source: s\n\
                listeners:\n  - name: l\n    bind: \"127.0.0.1:0\"\n    pool: p\n";
    let out = check(yaml, &[]);
    if cfg!(feature = "dns-srv") {
        assert!(out.status.success(), "{}", stderr(&out));
    } else {
        assert!(!out.status.success());
        assert!(
            stderr(&out).contains("`dns-srv` cargo feature"),
            "{}",
            stderr(&out)
        );
    }
}

#[cfg(not(feature = "tunnel"))]
mod without_tunnel {
    use super::*;

    #[test]
    fn the_tunnel_flags_are_refused_naming_the_feature() {
        let out = check(
            BASE,
            &[
                "--tunnel-iface",
                "gsp0",
                "--tunnel-controller-url",
                "http://127.0.0.1:1",
                "--tunnel-name",
                "p1",
                "--tunnel-endpoint",
                "203.0.113.1:51820",
            ],
        );
        assert!(!out.status.success());
        assert!(
            stderr(&out).contains("`tunnel` cargo feature"),
            "{}",
            stderr(&out)
        );
    }

    #[test]
    fn a_tunnel_source_is_refused_naming_the_feature() {
        let yaml = "backend_sources:\n  - name: s\n    type: tunnel\n    \
                    pubkey: \"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=\"\n\
                    pools:\n  - name: p\n    source: s\n\
                    listeners:\n  - name: l\n    bind: \"127.0.0.1:0\"\n    pool: p\n";
        let out = check(yaml, &[]);
        assert!(!out.status.success());
        assert!(
            stderr(&out).contains("`tunnel` cargo feature"),
            "{}",
            stderr(&out)
        );
    }
}
