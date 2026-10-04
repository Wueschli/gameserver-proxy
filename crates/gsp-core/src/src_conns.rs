//! Per-source **concurrent** connection / UDP-session cap (phase 7). Unlike the
//! `rate_limit` token bucket (which bounds the *rate* of new sessions), this
//! bounds how many are *live at once* from one client IP and/or its /24 (v4) /
//! /64 (v6) network. One [`SourceLimiter`] is shared across a listener's worker
//! tasks and rebuilt on respawn.
//!
//! A [`SourceGuard`] is held for the whole connection / session and releases
//! its two counters on drop (dropping a map entry that hits zero). Over the cap
//! ⇒ the new connection / session is refused before allocation, silently, and
//! counted as `gsp_filter_blocked_total{filter="src_conn_ip"|"src_conn_net"}`.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, PoisonError};

use gsp_config::PerSourceLimit;

use crate::ratelimit::NetKey;

#[derive(Debug, Default)]
struct Counts {
    ips: HashMap<IpAddr, usize>,
    nets: HashMap<NetKey, usize>,
}

#[derive(Debug)]
pub struct SourceLimiter {
    max_ip: Option<usize>,
    max_net: Option<usize>,
    counts: Mutex<Counts>,
}

impl SourceLimiter {
    pub fn new(cfg: Option<&PerSourceLimit>) -> Arc<Self> {
        Arc::new(Self {
            max_ip: cfg.and_then(|c| c.max_per_ip),
            max_net: cfg.and_then(|c| c.max_per_net),
            counts: Mutex::new(Counts::default()),
        })
    }

    pub fn is_enabled(&self) -> bool {
        self.max_ip.is_some() || self.max_net.is_some()
    }

    /// Reserve a live slot for a connection / session from `ip`. `Err(reason)`
    /// when a cap is already reached (nothing is reserved); `Ok(guard)` releases
    /// the slot on drop.
    pub fn acquire(self: &Arc<Self>, ip: IpAddr) -> Result<SourceGuard, &'static str> {
        if !self.is_enabled() {
            return Ok(SourceGuard { limiter: None });
        }
        let key = NetKey::of(ip);
        let mut c = self.counts.lock().unwrap_or_else(PoisonError::into_inner);

        if let Some(max) = self.max_ip {
            if c.ips.get(&ip).copied().unwrap_or(0) >= max {
                return Err("src_conn_ip");
            }
        }
        if let Some(max) = self.max_net {
            if c.nets.get(&key).copied().unwrap_or(0) >= max {
                return Err("src_conn_net");
            }
        }
        if self.max_ip.is_some() {
            *c.ips.entry(ip).or_insert(0) += 1;
        }
        if self.max_net.is_some() {
            *c.nets.entry(key).or_insert(0) += 1;
        }
        Ok(SourceGuard {
            limiter: Some((self.clone(), ip, key)),
        })
    }

    fn release(&self, ip: IpAddr, key: NetKey) {
        let mut c = self.counts.lock().unwrap_or_else(PoisonError::into_inner);
        if self.max_ip.is_some() {
            if let Some(n) = c.ips.get_mut(&ip) {
                *n -= 1;
                if *n == 0 {
                    c.ips.remove(&ip);
                }
            }
        }
        if self.max_net.is_some() {
            if let Some(n) = c.nets.get_mut(&key) {
                *n -= 1;
                if *n == 0 {
                    c.nets.remove(&key);
                }
            }
        }
    }
}

/// Holds one reserved per-source slot; releases it on drop. A guard from an
/// unconfigured limiter carries nothing.
#[derive(Debug)]
pub struct SourceGuard {
    limiter: Option<(Arc<SourceLimiter>, IpAddr, NetKey)>,
}

impl Drop for SourceGuard {
    fn drop(&mut self) {
        if let Some((limiter, ip, key)) = &self.limiter {
            limiter.release(*ip, *key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(ip: Option<usize>, net: Option<usize>) -> PerSourceLimit {
        PerSourceLimit {
            max_per_ip: ip,
            max_per_net: net,
        }
    }

    #[test]
    fn disabled_always_admits() {
        let l = SourceLimiter::new(None);
        assert!(!l.is_enabled());
        let mut g = Vec::new();
        for _ in 0..1000 {
            g.push(l.acquire("1.2.3.4".parse().unwrap()).unwrap());
        }
    }

    #[test]
    fn per_ip_cap_holds_and_releases_on_drop() {
        let l = SourceLimiter::new(Some(&cfg(Some(2), None)));
        let ip: IpAddr = "9.9.9.9".parse().unwrap();
        let g1 = l.acquire(ip).unwrap();
        let _g2 = l.acquire(ip).unwrap();
        assert_eq!(l.acquire(ip).unwrap_err(), "src_conn_ip");
        // A different IP is unaffected.
        let _other = l.acquire("9.9.9.10".parse::<IpAddr>().unwrap()).unwrap();
        drop(g1);
        let _g3 = l.acquire(ip).unwrap(); // slot freed
        assert_eq!(l.acquire(ip).unwrap_err(), "src_conn_ip");
    }

    #[test]
    fn per_net_cap_groups_the_24_and_wins_over_ip() {
        let l = SourceLimiter::new(Some(&cfg(Some(10), Some(2))));
        let _a = l.acquire("10.0.0.1".parse::<IpAddr>().unwrap()).unwrap();
        let _b = l.acquire("10.0.0.2".parse::<IpAddr>().unwrap()).unwrap();
        assert_eq!(
            l.acquire("10.0.0.3".parse::<IpAddr>().unwrap())
                .unwrap_err(),
            "src_conn_net"
        );
        // Another /24 is fine.
        let _c = l.acquire("10.0.1.1".parse::<IpAddr>().unwrap()).unwrap();
    }

    #[test]
    fn refusal_reserves_nothing() {
        let l = SourceLimiter::new(Some(&cfg(Some(1), Some(1))));
        let a: IpAddr = "172.16.5.5".parse().unwrap();
        let _g = l.acquire(a).unwrap();
        assert_eq!(l.acquire(a).unwrap_err(), "src_conn_ip");
        // The net counter must still be exactly 1 (the refused attempt didn't
        // bump it): a fresh IP in the same /24 is refused by net, not by a
        // double-counted value.
        let b: IpAddr = "172.16.5.6".parse().unwrap();
        assert_eq!(l.acquire(b).unwrap_err(), "src_conn_net");
        drop(_g);
        let _b2 = l.acquire(b).unwrap();
    }
}
