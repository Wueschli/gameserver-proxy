//! Stand-in for `dns_srv` in a build without the `dns-srv` feature (no
//! `hickory-resolver`). The type is uninhabited and `new` always refuses, so a
//! `dns_srv` backend source fails at startup (and under `--check`) instead of
//! silently never discovering anything.

use std::net::SocketAddr;
use std::time::Duration;

use async_trait::async_trait;
use gsp_core::{BackendSource, SourceError};

/// Never constructed; see the module doc.
pub enum DnsSrvSource {}

impl DnsSrvSource {
    #[allow(clippy::needless_pass_by_value)] // same signature as the real `new`
    pub fn new(pool: String, _record: String, _interval: Duration) -> anyhow::Result<Self> {
        anyhow::bail!(
            "pool {pool}: this build of gsp was compiled without the `dns-srv` cargo \
             feature, so it cannot use a `dns_srv` backend source; use `consul`, \
             `kubernetes` or a full build"
        )
    }
}

#[async_trait]
impl BackendSource for DnsSrvSource {
    fn pool(&self) -> &str {
        match *self {}
    }
    fn kind(&self) -> &'static str {
        match *self {}
    }
    fn refresh_interval(&self) -> Duration {
        match *self {}
    }
    async fn fetch(&self) -> Result<Vec<SocketAddr>, SourceError> {
        match *self {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dns_srv_source_is_refused_with_a_message_naming_the_feature() {
        let err = DnsSrvSource::new(
            "p".into(),
            "_game._udp.example.com.".into(),
            Duration::from_secs(10),
        )
        .err()
        .expect("must refuse")
        .to_string();
        assert!(err.contains("dns-srv"), "{err}");
    }
}
