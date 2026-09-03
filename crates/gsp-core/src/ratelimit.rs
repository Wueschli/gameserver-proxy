//! Per-listener token-bucket rate limiting on *new* connections / *new* UDP
//! sessions (phase 7). One [`RateLimiter`] is shared across all of a listener's
//! worker accept tasks; it is rebuilt whenever the listener respawns (a config
//! change), so the parameters always match the live [`ListenerConfig`].
//!
//! Keys: the client source IP, and its /24 (IPv4) or /64 (IPv6) network — the
//! network bucket catches floods spread thinly across many single IPs. A permit
//! is taken only when *both* configured buckets can afford it; on refusal
//! nothing is consumed. Excess is dropped silently (no error reply — the proxy
//! never reflects).
//!
//! First cut, each a drop-in replacement later: a single `Mutex<HashMap>` (brief
//! lock, never held across `.await`, like `Backend::observe`); lazy pruning of
//! idle (full) buckets when the map grows past a cap. A sharded map / timing
//! wheel can come in a perf pass.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;

use gsp_config::{RateLimit, TokenBucket as BucketCfg};

use crate::util::now_ms;

/// Drop the network-bucket map / IP-bucket map once either exceeds this many
/// entries, evicting buckets that have refilled to capacity (idle sources).
const PRUNE_AT: usize = 100_000;

#[derive(Clone, Copy)]
struct Bucket {
    tokens: f64,
    last_ms: u64,
}

impl Bucket {
    fn full(cfg: &BucketCfg, now: u64) -> Self {
        Self {
            tokens: cfg.burst as f64,
            last_ms: now,
        }
    }

    /// Refill for elapsed time, then report whether a whole permit is available.
    fn refill(&mut self, cfg: &BucketCfg, now: u64) {
        let dt = now.saturating_sub(self.last_ms) as f64 / 1000.0;
        self.last_ms = now;
        self.tokens = (self.tokens + dt * cfg.rate as f64).min(cfg.burst as f64);
    }

    fn is_full(&self, cfg: &BucketCfg) -> bool {
        self.tokens >= cfg.burst as f64
    }
}

/// Network key: the leading bytes of the address (/24 for v4, /64 for v6).
#[derive(PartialEq, Eq, Hash, Clone, Copy)]
enum NetKey {
    V4([u8; 3]),
    V6([u8; 8]),
}

impl NetKey {
    fn of(ip: IpAddr) -> Self {
        match ip {
            IpAddr::V4(a) => {
                let o = a.octets();
                NetKey::V4([o[0], o[1], o[2]])
            }
            IpAddr::V6(a) => {
                let o = a.octets();
                let mut p = [0u8; 8];
                p.copy_from_slice(&o[..8]);
                NetKey::V6(p)
            }
        }
    }
}

#[derive(Default)]
struct State {
    ips: HashMap<IpAddr, Bucket>,
    nets: HashMap<NetKey, Bucket>,
}

pub struct RateLimiter {
    per_ip: Option<BucketCfg>,
    per_net: Option<BucketCfg>,
    state: Mutex<State>,
}

impl RateLimiter {
    /// Build from a listener's resolved config. Returns a limiter that always
    /// admits when `cfg` is `None` (cheap: `permit` short-circuits).
    pub fn new(cfg: Option<&RateLimit>) -> Self {
        Self {
            per_ip: cfg.and_then(|c| c.per_ip),
            per_net: cfg.and_then(|c| c.per_net),
            state: Mutex::new(State::default()),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.per_ip.is_some() || self.per_net.is_some()
    }

    /// Try to take one permit for a connection / new session from `ip`.
    ///
    /// Returns `None` when admitted, or `Some("rate_ip" | "rate_net")` naming
    /// the bucket that refused (for the `gsp_filter_blocked_total` label). On
    /// refusal no tokens are consumed from either bucket.
    pub fn permit(&self, ip: IpAddr) -> Option<&'static str> {
        if !self.is_enabled() {
            return None;
        }
        let now = now_ms();
        let mut st = self.state.lock().unwrap();

        // Refill (creating a full bucket on first sight) and check affordability
        // for both buckets before consuming from either.
        let ip_ok = match &self.per_ip {
            None => true,
            Some(cfg) => {
                let b = st.ips.entry(ip).or_insert_with(|| Bucket::full(cfg, now));
                b.refill(cfg, now);
                b.tokens >= 1.0
            }
        };
        let net_ok = match &self.per_net {
            None => true,
            Some(cfg) => {
                let key = NetKey::of(ip);
                let b = st.nets.entry(key).or_insert_with(|| Bucket::full(cfg, now));
                b.refill(cfg, now);
                b.tokens >= 1.0
            }
        };

        if !ip_ok {
            self.maybe_prune(&mut st);
            return Some("rate_ip");
        }
        if !net_ok {
            self.maybe_prune(&mut st);
            return Some("rate_net");
        }

        if self.per_ip.is_some() {
            if let Some(b) = st.ips.get_mut(&ip) {
                b.tokens -= 1.0;
            }
        }
        if self.per_net.is_some() {
            if let Some(b) = st.nets.get_mut(&NetKey::of(ip)) {
                b.tokens -= 1.0;
            }
        }
        self.maybe_prune(&mut st);
        None
    }

    /// Bounded-memory guard: when a map gets large, drop the buckets that have
    /// refilled to capacity (sources that have gone quiet).
    fn maybe_prune(&self, st: &mut State) {
        if let Some(cfg) = &self.per_ip {
            if st.ips.len() > PRUNE_AT {
                st.ips.retain(|_, b| !b.is_full(cfg));
            }
        }
        if let Some(cfg) = &self.per_net {
            if st.nets.len() > PRUNE_AT {
                st.nets.retain(|_, b| !b.is_full(cfg));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rl(per_ip: Option<(u32, u32)>, per_net: Option<(u32, u32)>) -> RateLimiter {
        RateLimiter::new(Some(&RateLimit {
            per_ip: per_ip.map(|(rate, burst)| BucketCfg { rate, burst }),
            per_net: per_net.map(|(rate, burst)| BucketCfg { rate, burst }),
        }))
    }

    #[test]
    fn disabled_limiter_always_admits() {
        let l = RateLimiter::new(None);
        assert!(!l.is_enabled());
        for _ in 0..1000 {
            assert_eq!(l.permit("1.2.3.4".parse().unwrap()), None);
        }
    }

    #[test]
    fn per_ip_bucket_exhausts_then_refills() {
        // 10/s, burst 3.
        let l = rl(Some((10, 3)), None);
        let ip: IpAddr = "9.9.9.9".parse().unwrap();
        assert_eq!(l.permit(ip), None);
        assert_eq!(l.permit(ip), None);
        assert_eq!(l.permit(ip), None);
        assert_eq!(l.permit(ip), Some("rate_ip"));

        // Hand the bucket ~2 tokens back (as a refill would) and confirm it
        // grants exactly two more permits before refusing again.
        {
            let mut st = l.state.lock().unwrap();
            let b = st.ips.get_mut(&ip).unwrap();
            b.tokens = 2.0;
        }
        assert_eq!(l.permit(ip), None);
        assert_eq!(l.permit(ip), None);
        assert_eq!(l.permit(ip), Some("rate_ip"));
    }

    #[test]
    fn per_net_groups_addresses_in_the_same_24() {
        // No per-ip limit; /24 bucket of burst 2.
        let l = rl(None, Some((1, 2)));
        assert_eq!(l.permit("10.0.0.1".parse().unwrap()), None);
        assert_eq!(l.permit("10.0.0.2".parse().unwrap()), None);
        // Third distinct IP in 10.0.0.0/24 is refused by the shared net bucket.
        assert_eq!(l.permit("10.0.0.9".parse().unwrap()), Some("rate_net"));
        // A different /24 is unaffected.
        assert_eq!(l.permit("10.0.1.1".parse().unwrap()), None);
    }

    #[test]
    fn refusal_consumes_nothing() {
        // per_ip burst 1; per_net burst 1. First call drains both.
        let l = rl(Some((1, 1)), Some((1, 1)));
        let a: IpAddr = "172.16.5.5".parse().unwrap();
        assert_eq!(l.permit(a), None);
        assert_eq!(l.permit(a), Some("rate_ip"));
        // The /24 bucket still has its single token: a fresh IP in another /24
        // but same... no — use same /24, different ip: net refuses now too,
        // proving the earlier refusal didn't also consume the net token twice.
        let b: IpAddr = "172.16.5.6".parse().unwrap();
        assert_eq!(l.permit(b), Some("rate_net"));
    }
}
