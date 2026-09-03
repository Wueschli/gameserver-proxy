//! An immutable view of the running configuration.
//!
//! The runtime holds the current [`Snapshot`] behind an `ArcSwap`. The data
//! path only ever *reads* it; the control plane builds a new one with
//! [`Snapshot::build`] and swaps it in atomically (hot reload). Per-backend
//! health state is carried across the swap by address.

use std::collections::HashMap;
use std::sync::Arc;

use gsp_config::{Config, GlobalLimits, ListenerConfig};

use crate::overlay::BackendOverlay;
use crate::pool::Pool;

#[derive(Debug)]
pub struct Snapshot {
    pub listeners: Vec<ListenerConfig>,
    pub pools: HashMap<String, Arc<Pool>>,
    /// Process-wide caps (read once at `Runtime::start`; startup-only).
    pub limits: GlobalLimits,
    /// MaxMind Country DB path (read once at `Runtime::start`; startup-only).
    pub geo_db: Option<String>,
}

impl Snapshot {
    /// Build a snapshot, carrying over backend health from `prev` where a pool
    /// and backend address still exist.
    pub fn build(cfg: &Config, prev: Option<&Snapshot>) -> Arc<Self> {
        Self::build_with_overlay(cfg, prev, &BackendOverlay::new())
    }

    /// Like [`Snapshot::build`], but each pool's file `targets` are first run
    /// through the runtime [`BackendOverlay`] (admin-API backend add/remove).
    pub fn build_with_overlay(
        cfg: &Config,
        prev: Option<&Snapshot>,
        overlay: &BackendOverlay,
    ) -> Arc<Self> {
        let pools = cfg
            .pools
            .iter()
            .map(|pc| {
                let prev_pool = prev.and_then(|s| s.pools.get(&pc.name));
                let targets = overlay.effective_targets(&pc.name, &pc.targets);
                let pc = if targets == pc.targets {
                    pc.clone()
                } else {
                    let mut pc = pc.clone();
                    pc.targets = targets;
                    pc
                };
                (pc.name.clone(), Arc::new(Pool::new(&pc, prev_pool)))
            })
            .collect();
        Arc::new(Self {
            listeners: cfg.listeners.clone(),
            pools,
            limits: cfg.limits,
            geo_db: cfg.geo_db.clone(),
        })
    }

    /// Build a fresh snapshot with no carried-over state.
    pub fn from_config(cfg: &Config) -> Arc<Self> {
        Self::build(cfg, None)
    }

    pub fn pool(&self, name: &str) -> Option<Arc<Pool>> {
        self.pools.get(name).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const YAML: &str = r#"
pools:
  - name: p
    targets: ["127.0.0.1:9001", "127.0.0.1:9002"]
    health_check: { fall: 3 }
listeners:
  - name: l
    bind: "0.0.0.0:7777"
    pool: p
"#;

    #[test]
    fn overlay_adds_and_removes_backends_across_a_rebuild() {
        use crate::overlay::BackendOverlay;
        let cfg = gsp_config::parse_str(YAML).unwrap();
        let overlay = BackendOverlay::new();

        // Add a third backend and drop one of the file's.
        overlay.add("p", "127.0.0.1:9003".parse().unwrap());
        overlay.remove("p", "127.0.0.1:9001".parse().unwrap());

        let s = Snapshot::build_with_overlay(&cfg, None, &overlay);
        let addrs: Vec<String> = s
            .pool("p")
            .unwrap()
            .backends()
            .iter()
            .map(|b| b.addr.to_string())
            .collect();
        assert_eq!(addrs, vec!["127.0.0.1:9002", "127.0.0.1:9003"]);

        // Health carries across a further rebuild for the surviving addresses.
        s.pool("p").unwrap().backends()[0].observe(false);
        s.pool("p").unwrap().backends()[0].observe(false);
        s.pool("p").unwrap().backends()[0].observe(false);
        let s2 = Snapshot::build_with_overlay(&cfg, Some(&s), &overlay);
        assert!(!s2.pool("p").unwrap().backends()[0].is_healthy());
    }

    #[test]
    fn build_carries_backend_health_by_addr() {
        let cfg = gsp_config::parse_str(YAML).unwrap();
        let s1 = Snapshot::from_config(&cfg);
        let b = &s1.pool("p").unwrap().backends()[0].clone();
        for _ in 0..3 {
            b.observe(false);
        }
        assert!(!b.is_healthy());

        let s2 = Snapshot::build(&cfg, Some(&s1));
        assert!(
            !s2.pool("p").unwrap().backends()[0].is_healthy(),
            "unhealthy state should survive a reload"
        );
        assert!(
            s2.pool("p").unwrap().backends()[1].is_healthy(),
            "the other backend stays healthy"
        );
    }
}
