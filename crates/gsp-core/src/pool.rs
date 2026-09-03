//! A backend pool: a set of targets, per-backend health and load state, and a
//! selection strategy.
//!
//! [`Pool::acquire`] returns a [`BackendGuard`] that holds an active-session
//! slot for the lifetime of the connection (released on drop). Health state is
//! driven by the active checker ([`crate::health`]) and by passive connect
//! results reported through [`BackendGuard::observe`].

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gsp_config::{Balancer, HealthCheck, PoolConfig};

use crate::metrics_defs as m;

#[derive(Debug, thiserror::Error)]
pub enum PickError {
    #[error("no healthy backend in pool {0}")]
    NoHealthyBackend(String),
    #[error("all healthy backends in pool {0} are at capacity")]
    AllAtCapacity(String),
}

#[derive(Debug, Default)]
struct Streaks {
    ok: u32,
    fail: u32,
}

#[derive(Debug)]
pub struct Backend {
    pub addr: SocketAddr,
    pool: Arc<str>,
    healthy: AtomicBool,
    active: AtomicUsize,
    last_check_ms: AtomicU64,
    streaks: Mutex<Streaks>,
    rise: u32,
    fall: u32,
    check_interval: Duration,
    check_timeout: Duration,
    max_sessions: Option<usize>,
}

impl Backend {
    fn new(
        addr: SocketAddr,
        pool: Arc<str>,
        hc: &HealthCheck,
        max_sessions: Option<usize>,
        initially_healthy: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            addr,
            pool,
            healthy: AtomicBool::new(initially_healthy),
            active: AtomicUsize::new(0),
            last_check_ms: AtomicU64::new(0),
            streaks: Mutex::new(Streaks::default()),
            rise: hc.rise,
            fall: hc.fall,
            check_interval: hc.interval,
            check_timeout: hc.timeout,
            max_sessions,
        })
    }

    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Acquire)
    }

    pub fn active(&self) -> usize {
        self.active.load(Ordering::Relaxed)
    }

    pub fn check_timeout(&self) -> Duration {
        self.check_timeout
    }

    pub(crate) fn due_for_check(&self, now_ms: u64) -> bool {
        let last = self.last_check_ms.load(Ordering::Relaxed);
        last == 0 || now_ms.saturating_sub(last) >= self.check_interval.as_millis() as u64
    }

    pub(crate) fn mark_checked(&self, now_ms: u64) {
        self.last_check_ms.store(now_ms, Ordering::Relaxed);
    }

    /// Feed a health observation (active check result or passive connect
    /// result). Returns `Some(new_state)` when the healthy flag flips.
    pub fn observe(&self, ok: bool) -> Option<bool> {
        let mut s = self.streaks.lock().unwrap();
        if ok {
            s.fail = 0;
            s.ok = s.ok.saturating_add(1);
            if !self.healthy.load(Ordering::Acquire) && s.ok >= self.rise {
                self.healthy.store(true, Ordering::Release);
                return Some(true);
            }
        } else {
            s.ok = 0;
            s.fail = s.fail.saturating_add(1);
            if self.healthy.load(Ordering::Acquire) && s.fail >= self.fall {
                self.healthy.store(false, Ordering::Release);
                return Some(false);
            }
        }
        None
    }

    fn try_acquire(self: &Arc<Self>) -> Option<BackendGuard> {
        let prev = self.active.fetch_add(1, Ordering::Relaxed);
        if let Some(max) = self.max_sessions {
            if prev >= max {
                self.active.fetch_sub(1, Ordering::Relaxed);
                return None;
            }
        }
        metrics::gauge!(
            m::BACKEND_ACTIVE_SESSIONS,
            "pool" => self.pool.to_string(),
            "backend" => self.addr.to_string(),
        )
        .increment(1.0);
        Some(BackendGuard {
            backend: self.clone(),
        })
    }
}

/// Holds one active-session slot on a backend. Drop releases it.
#[derive(Debug)]
pub struct BackendGuard {
    backend: Arc<Backend>,
}

impl BackendGuard {
    pub fn addr(&self) -> SocketAddr {
        self.backend.addr
    }

    /// Report a connect result for passive health tracking.
    pub fn observe(&self, ok: bool) {
        if let Some(new_state) = self.backend.observe(ok) {
            tracing::info!(
                pool = %self.backend.pool,
                backend = %self.backend.addr,
                healthy = new_state,
                "backend health changed (passive)"
            );
        }
    }
}

impl Drop for BackendGuard {
    fn drop(&mut self) {
        self.backend.active.fetch_sub(1, Ordering::Relaxed);
        metrics::gauge!(
            m::BACKEND_ACTIVE_SESSIONS,
            "pool" => self.backend.pool.to_string(),
            "backend" => self.backend.addr.to_string(),
        )
        .decrement(1.0);
    }
}

#[derive(Debug)]
pub struct Pool {
    pub name: Arc<str>,
    pub balancer: Balancer,
    pub connect_timeout: Duration,
    pub idle_timeout: Duration,
    backends: Vec<Arc<Backend>>,
    rr: AtomicUsize,
}

impl Pool {
    /// Build a pool from config. `prev` (the same pool from the previous
    /// snapshot, if any) is consulted to carry over per-backend health state
    /// by address across a hot reload.
    pub fn new(cfg: &PoolConfig, prev: Option<&Arc<Pool>>) -> Self {
        let name: Arc<str> = Arc::from(cfg.name.as_str());
        let backends = cfg
            .targets
            .iter()
            .map(|&addr| {
                let carried_healthy = prev
                    .and_then(|p| p.backends.iter().find(|b| b.addr == addr))
                    .map(|b| b.is_healthy())
                    .unwrap_or(true);
                Backend::new(
                    addr,
                    name.clone(),
                    &cfg.health_check,
                    cfg.max_sessions,
                    carried_healthy,
                )
            })
            .collect();
        Self {
            name,
            balancer: cfg.balancer,
            connect_timeout: cfg.connect_timeout,
            idle_timeout: cfg.idle_timeout,
            backends,
            rr: AtomicUsize::new(0),
        }
    }

    pub fn backends(&self) -> &[Arc<Backend>] {
        &self.backends
    }

    /// Select a healthy backend with free capacity and reserve a session slot.
    pub fn acquire(&self) -> Result<BackendGuard, PickError> {
        let mut healthy: Vec<&Arc<Backend>> =
            self.backends.iter().filter(|b| b.is_healthy()).collect();
        if healthy.is_empty() {
            metrics::counter!(
                m::LB_SELECTIONS,
                "pool" => self.name.to_string(),
                "strategy" => strategy_str(self.balancer),
                "result" => "no_backend",
            )
            .increment(1);
            return Err(PickError::NoHealthyBackend(self.name.to_string()));
        }

        match self.balancer {
            Balancer::LeastConn => healthy.sort_by_key(|b| b.active()),
            Balancer::RoundRobin => {
                let start = self.rr.fetch_add(1, Ordering::Relaxed) % healthy.len();
                healthy.rotate_left(start);
            }
        }

        for b in healthy {
            if let Some(guard) = b.try_acquire() {
                metrics::counter!(
                    m::LB_SELECTIONS,
                    "pool" => self.name.to_string(),
                    "strategy" => strategy_str(self.balancer),
                    "result" => "ok",
                )
                .increment(1);
                return Ok(guard);
            }
        }

        metrics::counter!(
            m::LB_SELECTIONS,
            "pool" => self.name.to_string(),
            "strategy" => strategy_str(self.balancer),
            "result" => "at_capacity",
        )
        .increment(1);
        Err(PickError::AllAtCapacity(self.name.to_string()))
    }
}

fn strategy_str(b: Balancer) -> &'static str {
    match b {
        Balancer::RoundRobin => "round_robin",
        Balancer::LeastConn => "least_conn",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcfg(targets: &[&str], balancer: Balancer, max: Option<usize>) -> PoolConfig {
        PoolConfig {
            name: "t".into(),
            targets: targets.iter().map(|s| s.parse().unwrap()).collect(),
            balancer,
            connect_timeout: Duration::from_millis(300),
            idle_timeout: Duration::from_secs(90),
            health_check: HealthCheck {
                interval: Duration::from_secs(2),
                timeout: Duration::from_millis(500),
                rise: 2,
                fall: 3,
            },
            max_sessions: max,
        }
    }

    #[test]
    fn round_robin_cycles_through_targets() {
        let p = Pool::new(
            &pcfg(
                &["127.0.0.1:1", "127.0.0.1:2", "127.0.0.1:3"],
                Balancer::RoundRobin,
                None,
            ),
            None,
        );
        let seq: Vec<u16> = (0..6).map(|_| p.acquire().unwrap().addr().port()).collect();
        assert_eq!(seq, vec![1, 2, 3, 1, 2, 3]);
    }

    #[test]
    fn least_conn_prefers_the_idle_backend() {
        let p = Pool::new(
            &pcfg(&["127.0.0.1:1", "127.0.0.1:2"], Balancer::LeastConn, None),
            None,
        );
        let g1 = p.acquire().unwrap(); // one backend now has 1 active session
        let busy = g1.addr().port();
        let g2 = p.acquire().unwrap(); // must go to the other (0 < 1)
        assert_ne!(
            g2.addr().port(),
            busy,
            "second session should pick the idle backend"
        );
    }

    #[test]
    fn respects_per_backend_capacity() {
        let p = Pool::new(&pcfg(&["127.0.0.1:1"], Balancer::RoundRobin, Some(1)), None);
        let _g = p.acquire().unwrap();
        let err = p.acquire().unwrap_err();
        assert!(matches!(err, PickError::AllAtCapacity(_)), "got {err:?}");
    }

    #[test]
    fn unhealthy_backends_are_skipped() {
        let p = Pool::new(
            &pcfg(&["127.0.0.1:1", "127.0.0.1:2"], Balancer::RoundRobin, None),
            None,
        );
        for _ in 0..3 {
            p.backends()[0].observe(false); // fall = 3
        }
        assert!(!p.backends()[0].is_healthy());
        for _ in 0..4 {
            assert_eq!(p.acquire().unwrap().addr().port(), 2);
        }
    }

    #[test]
    fn all_unhealthy_yields_error() {
        let p = Pool::new(&pcfg(&["127.0.0.1:1"], Balancer::RoundRobin, None), None);
        for _ in 0..3 {
            p.backends()[0].observe(false);
        }
        assert!(matches!(
            p.acquire().unwrap_err(),
            PickError::NoHealthyBackend(_)
        ));
    }

    #[test]
    fn observe_applies_rise_and_fall_thresholds() {
        let p = Pool::new(&pcfg(&["127.0.0.1:1"], Balancer::RoundRobin, None), None);
        let b = &p.backends()[0];
        assert_eq!(b.observe(false), None); // 1 fail
        assert_eq!(b.observe(false), None); // 2 fails
        assert_eq!(b.observe(false), Some(false)); // 3 fails -> down
        assert_eq!(b.observe(true), None); // 1 ok
        assert_eq!(b.observe(true), Some(true)); // 2 oks -> up (rise = 2)
    }
}
