//! An immutable view of the running configuration.
//!
//! The runtime holds the current [`Snapshot`] behind an `ArcSwap`. The data
//! path only ever *reads* it; the control plane builds a new one and swaps it
//! in atomically (hot reload — a later slice).

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
    pub fn from_config(cfg: &Config) -> Arc<Self> {
        let pools = cfg
            .pools
            .iter()
            .map(|p| (p.name.clone(), Arc::new(Pool::new(p.clone()))))
            .collect();
        Arc::new(Self {
            listeners: cfg.listeners.clone(),
            pools,
        })
    }

    pub fn pool(&self, name: &str) -> Option<Arc<Pool>> {
        self.pools.get(name).cloned()
    }
}
