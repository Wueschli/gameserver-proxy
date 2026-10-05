// Shared by `build.rs` (which uses it) and, under `cfg(test)`, by `lib.rs` (so
// `cargo test` covers it: tests inside a build script are never run).

/// Length of the short SHA reported in `gsp_build_info`.
pub const SHORT_LEN: usize = 12;

/// The commit to report: `GSP_GIT_SHA` from the environment wins (Docker builds
/// have no `.git`; CI and `deploy/build-images.sh` pass it in), then the output
/// of `git rev-parse`, then `"unknown"`. Blank values count as unset.
pub fn pick_sha(env: Option<&str>, git: Option<&str>) -> String {
    [env, git]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|s| !s.is_empty())
        .map(|s| s.chars().take(SHORT_LEN).collect())
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::pick_sha;

    #[test]
    fn env_wins_over_git() {
        assert_eq!(pick_sha(Some("abc123"), Some("def456")), "abc123");
    }

    #[test]
    fn falls_back_to_git_then_unknown() {
        assert_eq!(pick_sha(None, Some("def456")), "def456");
        assert_eq!(pick_sha(None, None), "unknown");
    }

    #[test]
    fn blank_env_counts_as_unset() {
        assert_eq!(pick_sha(Some("  \n"), Some("def456")), "def456");
        assert_eq!(pick_sha(Some(""), None), "unknown");
    }

    #[test]
    fn a_full_sha_is_shortened_to_twelve() {
        let full = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(pick_sha(Some(full), None), "0123456789ab");
    }
}
