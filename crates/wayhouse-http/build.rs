#[path = "build_sha.rs"]
mod build_sha;

use std::process::Command;

/// Run `git <args>` and return its trimmed stdout, if it succeeded.
fn git(args: &[&str]) -> Option<String> {
    Command::new("git")
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
}

fn main() {
    // Short git commit SHA for `wayhouse_build_info` (docs/06), shared by every
    // binary through `wayhouse_http::COMMIT` (and in `--version`). `WAYHOUSE_GIT_SHA` wins: a Docker
    // build has no `.git` (it is in `.dockerignore`), so CI and
    // `deploy/build-images.sh` pass it as a build arg. Otherwise `git rev-parse`,
    // otherwise `commit="unknown"` (a source tarball still builds).
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=build_sha.rs");
    println!("cargo:rerun-if-env-changed=WAYHOUSE_GIT_SHA");
    // On a branch `.git/HEAD` holds `ref: refs/heads/<name>` and does not change
    // between commits; the reflog does (every commit, checkout and reset appends).
    // Ask git for the paths so a worktree (where `.git` is a file) works too.
    for p in ["HEAD", "logs/HEAD"] {
        if let Some(path) = git(&["rev-parse", "--git-path", p]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    let env = std::env::var("WAYHOUSE_GIT_SHA").ok();
    let rev = git(&[
        "rev-parse",
        &format!("--short={}", build_sha::SHORT_LEN),
        "HEAD",
    ]);
    let sha = build_sha::pick_sha(env.as_deref(), rev.as_deref());
    println!("cargo:rustc-env=WAYHOUSE_GIT_SHA={sha}");
    // `--version` of every binary (all share the workspace version): `<version> (<sha>)`.
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    println!("cargo:rustc-env=WAYHOUSE_LONG_VERSION={version} ({sha})");
}
