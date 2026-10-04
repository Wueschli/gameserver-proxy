fn main() {
    // Short git commit SHA for `gsp_build_info` (docs/06), shared by every
    // binary through `gsp_http::metrics::COMMIT`. Best-effort: a source tarball
    // / shallow checkout with no `.git` still builds, just with
    // `commit="unknown"`. Re-run only when HEAD actually moves, not on every
    // unrelated file change.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    let sha = std::process::Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=GSP_GIT_SHA={sha}");
}
