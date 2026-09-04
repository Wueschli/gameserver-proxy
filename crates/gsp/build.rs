fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto/resolver.proto");
    tonic_build::configure()
        .build_client(true)
        .build_server(true) // the server stub is used by this crate's own tests
        .compile_protos(&["proto/resolver.proto"], &["proto"])?;

    // Short git commit SHA for `gsp_build_info` (docs/06). Best-effort: a
    // source tarball / shallow checkout with no `.git` still builds, just
    // with `commit="unknown"`. Re-run only when HEAD actually moves, not on
    // every unrelated file change.
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

    Ok(())
}
