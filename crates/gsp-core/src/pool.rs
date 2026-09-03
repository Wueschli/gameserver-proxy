//! A backend pool: a set of equivalent targets plus a selection strategy.
//!
//! v0 has no health state, so [`Pool::pick`] always succeeds for a non-empty
//! pool (config validation guarantees at least one target). Health checks and
//! backend states arrive in a later slice.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use gsp_config::{Balancer, PoolConfig};

#[derive(Debug)]
pub struct Pool {
    pub name: String,
    pub balancer: Balancer,
    pub connect_timeout: Duration,
    pub idle_timeout: Duration,
    targets: Vec<SocketAddr>,
    rr: AtomicUsize,
}

impl Pool {
    pub fn new(cfg: PoolConfig) -> Self {
        Self {
            name: cfg.name,
            balancer: cfg.balancer,
            connect_timeout: cfg.connect_timeout,
            idle_timeout: cfg.idle_timeout,
            targets: cfg.targets,
            rr: AtomicUsize::new(0),
        }
    }

    /// Pick the next backend according to the pool's strategy.
    pub fn pick(&self) -> SocketAddr {
        match self.balancer {
            Balancer::RoundRobin => {
                let i = self.rr.fetch_add(1, Ordering::Relaxed);
                self.targets[i % self.targets.len()]
            }
        }
    }

    pub fn targets(&self) -> &[SocketAddr] {
        &self.targets
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(targets: &[&str]) -> Pool {
        Pool::new(PoolConfig {
            name: "t".into(),
            targets: targets.iter().map(|s| s.parse().unwrap()).collect(),
            balancer: Balancer::RoundRobin,
            connect_timeout: Duration::from_millis(300),
            idle_timeout: Duration::from_secs(90),
        })
    }

    #[test]
    fn round_robin_cycles_through_targets() {
        let p = pool(&["127.0.0.1:1", "127.0.0.1:2", "127.0.0.1:3"]);
        let seq: Vec<u16> = (0..6).map(|_| p.pick().port()).collect();
        assert_eq!(seq, vec![1, 2, 3, 1, 2, 3]);
    }

    #[test]
    fn single_target_always_picked() {
        let p = pool(&["127.0.0.1:7"]);
        assert_eq!(p.pick().port(), 7);
        assert_eq!(p.pick().port(), 7);
    }
}
