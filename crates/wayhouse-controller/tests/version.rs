//! `--version` carries the commit this binary was built from, so
//! `deploy/check-image-commit.sh` can verify every image (issue #159).

#[test]
fn version_reports_the_build_commit() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_wayhouse-controller"))
        .arg("--version")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.trim_end().ends_with(wayhouse_http::LONG_VERSION),
        "unexpected --version output: {text}"
    );
    assert!(text.contains(wayhouse_http::COMMIT), "no commit in: {text}");
}
