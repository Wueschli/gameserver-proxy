//! Stand-in for `gossip` in a build without the `gossip` feature (no `foca`,
//! `postcard` or `hmac`). [`spawn`] starts nothing, so `health.rs` never gets a
//! [`GossipFabric`] and keeps its local-only verdicts; the types exist only so
//! `health.rs` compiles unchanged. `gsp` refuses `settings.gossip` at startup
//! (and under `--check`), so the error `spawn` logs is a backstop for another
//! embedder of this crate.

use gsp_config::GossipConfig;
use tokio::sync::watch;

/// Never constructed; see the module doc.
#[derive(Clone)]
pub enum GossipHandle {}

impl GossipHandle {
    pub fn publish_backend_health(&self, _addr: std::net::SocketAddr, _up: bool) {
        match *self {}
    }

    pub fn quorum_down(&self, _addr: std::net::SocketAddr, _quorum_fraction: f64) -> bool {
        match *self {}
    }
}

/// Never constructed; see the module doc.
#[derive(Clone)]
pub struct GossipFabric {
    pub handle: GossipHandle,
    pub quorum_fraction: f64,
}

/// Refuses: logs that the mesh cannot run in this build and starts nothing.
#[allow(clippy::needless_pass_by_value)] // same signature as the real `spawn`
pub fn spawn(
    cfg: GossipConfig,
    _shutdown: watch::Receiver<bool>,
) -> Option<(GossipFabric, tokio::task::JoinHandle<()>)> {
    tracing::error!(
        bind = %cfg.bind,
        "settings.gossip is set, but gsp-core was built without the `gossip` cargo \
         feature: no regional health fabric runs"
    );
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_starts_nothing_without_the_feature() {
        let cfg = GossipConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            seeds: vec![],
            quorum_fraction: 0.66,
            psk: "test-psk".to_string(),
        };
        let (_tx, rx) = watch::channel(false);
        assert!(spawn(cfg, rx).is_none());
    }
}
