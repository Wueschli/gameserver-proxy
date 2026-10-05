//! Stand-in for `tunnel_source` in a build without the `tunnel` feature. There is
//! no tunnel registry to resolve against (`tunnel_boot_disabled.rs` refuses the
//! `--tunnel-*` flags that would name one), so a `tunnel` backend source fails at
//! startup (and under `--check`) instead of silently never discovering anything.

use std::sync::Arc;
use std::time::Duration;

use wayhouse_core::BackendSource;

/// Never constructed; see the module doc.
#[derive(Debug, Clone)]
pub enum TunnelRegistry {}

#[allow(clippy::needless_pass_by_value)] // same signature as the real `build`
pub fn build(
    pool: String,
    source: &str,
    _pubkey: &str,
    _registry: Option<&TunnelRegistry>,
    _interval: Duration,
) -> anyhow::Result<Arc<dyn BackendSource>> {
    anyhow::bail!(
        "pool {pool}: backend_sources {source}: this build of wayhouse was compiled without the \
         `tunnel` cargo feature, so it cannot use a `tunnel` backend source; use a full build"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tunnel_source_is_refused_with_a_message_naming_the_feature() {
        let err = build("p".into(), "home", "key", None, Duration::from_secs(10))
            .err()
            .expect("must refuse")
            .to_string();
        assert!(err.contains("`tunnel` cargo feature"), "{err}");
    }
}
