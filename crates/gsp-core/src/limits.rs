//! Process-wide resource caps (phase 7, filter-chain step 4): a ceiling on live
//! TCP connections, a ceiling on live UDP sessions, and a token-bucket rate on
//! *new* connections + sessions per second. One [`GlobalLimits`] is built once
//! at [`crate::runtime::Runtime::start`] and shared by every listener worker;
//! the values are startup-only (like `settings.workers`) — a reload does not
//! change them.
//!
//! When a cap is hit the new connection / session is refused *before* any
//! buffer or task is allocated; established ones are untouched. A refusal is
//! silent (no error reply — the proxy never reflects) and shows up as
//! `gsp_filter_blocked_total{filter="max_conn"|"max_udp"|"max_new_rate"}`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use gsp_config::GlobalLimits as LimitsCfg;

use crate::util::now_ms;

/// Which resource a permit / refusal refers to.
#[derive(Debug, Clone, Copy)]
enum Kind {
    Tcp,
    Udp,
}

#[derive(Debug)]
struct NewRate {
    tokens: f64,
    last_ms: u64,
    rate: f64,
    burst: f64,
}

impl NewRate {
    fn take(&mut self) -> bool {
        let now = now_ms();
        let dt = now.saturating_sub(self.last_ms) as f64 / 1000.0;
        self.last_ms = now;
        self.tokens = (self.tokens + dt * self.rate).min(self.burst);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

#[derive(Debug)]
pub struct GlobalLimits {
    max_conn: Option<usize>,
    max_udp: Option<usize>,
    tcp_live: AtomicUsize,
    udp_live: AtomicUsize,
    new_rate: Option<Mutex<NewRate>>,
}

impl GlobalLimits {
    pub fn new(cfg: &LimitsCfg) -> Arc<Self> {
        let now = now_ms();
        Arc::new(Self {
            max_conn: cfg.max_connections,
            max_udp: cfg.max_udp_sessions,
            tcp_live: AtomicUsize::new(0),
            udp_live: AtomicUsize::new(0),
            new_rate: cfg.max_new_sessions_per_sec.map(|r| {
                Mutex::new(NewRate {
                    tokens: r as f64,
                    last_ms: now,
                    rate: r as f64,
                    burst: r as f64,
                })
            }),
        })
    }

    pub fn is_enabled(&self) -> bool {
        self.max_conn.is_some() || self.max_udp.is_some() || self.new_rate.is_some()
    }

    /// Reserve a slot for a new TCP connection. `Err(reason)` when a cap is hit
    /// (nothing is consumed); `Ok(guard)` releases the slot on drop.
    pub fn acquire_tcp(self: &Arc<Self>) -> Result<LimitGuard, &'static str> {
        self.acquire(Kind::Tcp)
    }

    /// Reserve a slot for a new UDP session. See [`GlobalLimits::acquire_tcp`].
    pub fn acquire_udp(self: &Arc<Self>) -> Result<LimitGuard, &'static str> {
        self.acquire(Kind::Udp)
    }

    fn acquire(self: &Arc<Self>, kind: Kind) -> Result<LimitGuard, &'static str> {
        if !self.is_enabled() {
            return Ok(LimitGuard { limits: None });
        }
        let (counter, max, over_label) = match kind {
            Kind::Tcp => (&self.tcp_live, self.max_conn, "max_conn"),
            Kind::Udp => (&self.udp_live, self.max_udp, "max_udp"),
        };

        // Count cap first (reversible with a fetch_sub), then the new-rate
        // bucket, so a refusal on either side leaks nothing.
        if let Some(max) = max {
            let prev = counter.fetch_add(1, Ordering::AcqRel);
            if prev >= max {
                counter.fetch_sub(1, Ordering::AcqRel);
                return Err(over_label);
            }
        }
        if let Some(rate) = &self.new_rate {
            if !rate.lock().unwrap().take() {
                if max.is_some() {
                    counter.fetch_sub(1, Ordering::AcqRel);
                }
                return Err("max_new_rate");
            }
        }
        Ok(LimitGuard {
            limits: max.map(|_| (self.clone(), kind)),
        })
    }

    #[cfg(test)]
    fn live(&self, kind: Kind) -> usize {
        match kind {
            Kind::Tcp => self.tcp_live.load(Ordering::Acquire),
            Kind::Udp => self.udp_live.load(Ordering::Acquire),
        }
    }
}

/// Holds one reserved connection / session slot; releases it on drop. A guard
/// from an uncapped resource carries nothing.
#[derive(Debug)]
pub struct LimitGuard {
    limits: Option<(Arc<GlobalLimits>, Kind)>,
}

impl Drop for LimitGuard {
    fn drop(&mut self) {
        if let Some((limits, kind)) = &self.limits {
            let counter = match kind {
                Kind::Tcp => &limits.tcp_live,
                Kind::Udp => &limits.udp_live,
            };
            counter.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(c: Option<usize>, u: Option<usize>, r: Option<u32>) -> LimitsCfg {
        LimitsCfg {
            max_connections: c,
            max_udp_sessions: u,
            max_new_sessions_per_sec: r,
        }
    }

    #[test]
    fn disabled_limits_always_admit() {
        let l = GlobalLimits::new(&cfg(None, None, None));
        assert!(!l.is_enabled());
        let mut guards = Vec::new();
        for _ in 0..10_000 {
            guards.push(l.acquire_tcp().unwrap());
        }
    }

    #[test]
    fn max_connections_caps_live_tcp_and_releases_on_drop() {
        let l = GlobalLimits::new(&cfg(Some(2), None, None));
        let g1 = l.acquire_tcp().unwrap();
        let g2 = l.acquire_tcp().unwrap();
        assert_eq!(l.acquire_tcp().unwrap_err(), "max_conn");
        assert_eq!(l.live(Kind::Tcp), 2);
        drop(g1);
        let _g3 = l.acquire_tcp().unwrap();
        drop(g2);
        assert_eq!(l.live(Kind::Tcp), 1);
    }

    #[test]
    fn tcp_and_udp_caps_are_independent() {
        let l = GlobalLimits::new(&cfg(Some(1), Some(1), None));
        let _t = l.acquire_tcp().unwrap();
        let _u = l.acquire_udp().unwrap();
        assert_eq!(l.acquire_tcp().unwrap_err(), "max_conn");
        assert_eq!(l.acquire_udp().unwrap_err(), "max_udp");
    }

    #[test]
    fn new_rate_bucket_limits_bursts_and_does_not_leak_the_count() {
        // burst 2 new/sec, generous connection cap.
        let l = GlobalLimits::new(&cfg(Some(100), None, Some(2)));
        let _a = l.acquire_tcp().unwrap();
        let _b = l.acquire_tcp().unwrap();
        assert_eq!(l.acquire_tcp().unwrap_err(), "max_new_rate");
        // The refused attempt must not have consumed a connection slot.
        assert_eq!(l.live(Kind::Tcp), 2);
    }
}
