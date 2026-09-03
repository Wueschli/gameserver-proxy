//! An immutable view of the running configuration.
//!
//! The runtime holds the current [`Snapshot`] behind an `ArcSwap`. The data
//! path only ever *reads* it; the control plane builds a new one with
//! [`Snapshot::build`] and swaps it in atomically (hot reload). Per-backend
//! health state is carried across the swap by address.

use std::collections::HashMap;
use std::sync::Arc;

use gsp_config::{Config, ListenerConfig};

use crate::pool::Pool;

#[derive(Debug)]
pub struct Snapshot {
    pub listeners: Vec<ListenerConfig>,
    pub pools: HashMap<String, Arc<Pool>>,
}

impl Snapshot {
    /// Build a snapshot, carrying over backend health from `prev` where a pool
    /// and backend address still exist.
    pub fn build(cfg: &Config, prev: Option<&Snapshot>) -> Arc<Self> {
        let pools = cfg
            .pools
            .iter()
            .map(|pc| {
                let prev_pool = prev.and_then(|s| s.pools.get(&pc.name));
                (pc.name.clone(), Arc::new(Pool::new(pc, prev_pool)))
            })
            .collect();
        Arc::new(Self {
            listeners: cfg.listeners.clone(),
            pools,
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
