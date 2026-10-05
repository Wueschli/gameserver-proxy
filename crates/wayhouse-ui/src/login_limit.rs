//! In-process rate limit for `POST /ui/login`. Every attempt costs an Argon2
//! verification in `--users-file` mode, so an unthrottled login is both a
//! brute-force target and a cheap CPU DoS. Two token buckets per attempt:
//! one per client address (IPv6 by /64) and one per username (so a botnet
//! can't spread guesses at one account). Behind a reverse proxy every client
//! shares the proxy's address and the per-IP bucket degrades to a global one;
//! the per-username bucket still holds.
//!
//! Ephemeral like the session store: a restart forgets everything.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Bucket shape: `burst` attempts at once, then one per `refill`.
#[derive(Debug, Clone, Copy)]
pub struct Bucket {
    pub burst: u32,
    pub refill: Duration,
}

#[derive(Debug, Clone, Copy)]
pub struct LoginLimits {
    pub per_ip: Bucket,
    pub per_username: Bucket,
    /// Most tracked keys per map; at the cap new keys are refused (fail
    /// closed) once idle ones have been pruned.
    pub max_keys: usize,
}

impl Default for LoginLimits {
    fn default() -> Self {
        LoginLimits {
            per_ip: Bucket {
                burst: 10,
                refill: Duration::from_secs(3),
            },
            per_username: Bucket {
                burst: 5,
                refill: Duration::from_secs(12),
            },
            max_keys: 10_000,
        }
    }
}

struct State {
    tokens: f64,
    at: Instant,
}

struct Buckets<K> {
    map: HashMap<K, State>,
}

impl<K> Default for Buckets<K> {
    fn default() -> Self {
        Buckets {
            map: HashMap::new(),
        }
    }
}

impl<K: std::hash::Hash + Eq + Clone> Buckets<K> {
    /// Takes one token for `key`; `Err(wait)` when the bucket is empty.
    /// Does not mutate on refusal, so a flood of refused requests costs
    /// the victim key nothing extra.
    fn take(&mut self, key: &K, b: Bucket, max_keys: usize, now: Instant) -> Result<(), Duration> {
        let rate = 1.0 / b.refill.as_secs_f64();
        if !self.map.contains_key(key) && self.map.len() >= max_keys {
            // Full buckets are indistinguishable from unseen keys; drop them.
            self.map
                .retain(|_, s| Self::level(s, rate, b, now) < f64::from(b.burst));
            if self.map.len() >= max_keys {
                return Err(b.refill);
            }
        }
        let s = self.map.entry(key.clone()).or_insert(State {
            tokens: f64::from(b.burst),
            at: now,
        });
        let level = Self::level(s, rate, b, now);
        if level < 1.0 {
            return Err(Duration::from_secs_f64((1.0 - level) / rate));
        }
        s.tokens = level - 1.0;
        s.at = now;
        Ok(())
    }

    fn level(s: &State, rate: f64, b: Bucket, now: Instant) -> f64 {
        let gained = now.saturating_duration_since(s.at).as_secs_f64() * rate;
        (s.tokens + gained).min(f64::from(b.burst))
    }

    /// Hands a token back (used when the second bucket refuses).
    fn give_back(&mut self, key: &K) {
        if let Some(s) = self.map.get_mut(key) {
            s.tokens += 1.0;
        }
    }
}

pub struct LoginLimiter {
    limits: LoginLimits,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    ips: Buckets<IpAddr>,
    users: Buckets<String>,
}

impl Default for LoginLimiter {
    fn default() -> Self {
        Self::new(LoginLimits::default())
    }
}

/// An IPv4-mapped IPv6 address is its IPv4 address; other IPv6 addresses
/// collapse to their /64 so one host can't dodge the limit by rotating
/// addresses inside its prefix.
fn source_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => {
                let o = v6.octets();
                let mut masked = [0u8; 16];
                masked[..8].copy_from_slice(&o[..8]);
                IpAddr::V6(Ipv6Addr::from(masked))
            }
        },
    }
}

impl LoginLimiter {
    pub fn new(limits: LoginLimits) -> Self {
        LoginLimiter {
            limits,
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Admits one login attempt from `ip` for `username` (if any), or says
    /// how long to wait.
    pub fn check(&self, ip: IpAddr, username: Option<&str>) -> Result<(), Duration> {
        self.check_at(ip, username, Instant::now())
    }

    fn check_at(&self, ip: IpAddr, username: Option<&str>, now: Instant) -> Result<(), Duration> {
        let ip = source_key(ip);
        let l = self.limits;
        let mut g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        g.ips.take(&ip, l.per_ip, l.max_keys, now)?;
        if let Some(name) = username {
            let name = name.to_lowercase();
            if let Err(wait) = g.users.take(&name, l.per_username, l.max_keys, now) {
                g.ips.give_back(&ip);
                return Err(wait);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn limiter(ip_burst: u32, user_burst: u32, max_keys: usize) -> LoginLimiter {
        LoginLimiter::new(LoginLimits {
            per_ip: Bucket {
                burst: ip_burst,
                refill: Duration::from_secs(10),
            },
            per_username: Bucket {
                burst: user_burst,
                refill: Duration::from_secs(10),
            },
            max_keys,
        })
    }

    #[test]
    fn an_ip_is_refused_after_its_burst_and_admitted_again_after_a_refill() {
        let l = limiter(2, 100, 100);
        let t0 = Instant::now();
        assert!(l.check_at(ip("192.0.2.1"), None, t0).is_ok());
        assert!(l.check_at(ip("192.0.2.1"), None, t0).is_ok());
        let wait = l.check_at(ip("192.0.2.1"), None, t0).unwrap_err();
        assert!(wait > Duration::ZERO && wait <= Duration::from_secs(10));
        assert!(l.check_at(ip("192.0.2.2"), None, t0).is_ok(), "other IP");
        let later = t0 + Duration::from_secs(10);
        assert!(l.check_at(ip("192.0.2.1"), None, later).is_ok());
    }

    #[test]
    fn a_username_is_limited_across_many_ips_case_insensitively() {
        let l = limiter(100, 2, 100);
        let t0 = Instant::now();
        assert!(l.check_at(ip("192.0.2.1"), Some("Alice"), t0).is_ok());
        assert!(l.check_at(ip("192.0.2.2"), Some("alice"), t0).is_ok());
        assert!(l.check_at(ip("192.0.2.3"), Some("ALICE"), t0).is_err());
        assert!(l.check_at(ip("192.0.2.3"), Some("bob"), t0).is_ok());
    }

    #[test]
    fn a_refused_username_does_not_burn_the_ips_budget() {
        let l = limiter(2, 1, 100);
        let t0 = Instant::now();
        assert!(l.check_at(ip("192.0.2.1"), Some("alice"), t0).is_ok());
        assert!(l.check_at(ip("192.0.2.1"), Some("alice"), t0).is_err());
        // The refused attempt gave its IP token back: one is left.
        assert!(l.check_at(ip("192.0.2.1"), Some("bob"), t0).is_ok());
    }

    #[test]
    fn an_ipv6_prefix_shares_one_bucket_and_mapped_v4_is_its_v4() {
        let l = limiter(1, 100, 100);
        let t0 = Instant::now();
        assert!(l.check_at(ip("2001:db8:1:2::1"), None, t0).is_ok());
        assert!(l.check_at(ip("2001:db8:1:2:ffff::9"), None, t0).is_err());
        assert!(l.check_at(ip("2001:db8:1:3::1"), None, t0).is_ok());
        assert!(l.check_at(ip("::ffff:192.0.2.1"), None, t0).is_ok());
        assert!(l.check_at(ip("192.0.2.1"), None, t0).is_err());
    }

    #[test]
    fn tracked_keys_are_capped_and_idle_ones_pruned() {
        let l = limiter(5, 100, 2);
        let t0 = Instant::now();
        assert!(l.check_at(ip("192.0.2.1"), None, t0).is_ok());
        assert!(l.check_at(ip("192.0.2.2"), None, t0).is_ok());
        // Map full of keys still below burst: a new key is refused.
        assert!(l.check_at(ip("192.0.2.3"), None, t0).is_err());
        // Once the old buckets have refilled they are pruned to make room.
        let later = t0 + Duration::from_secs(60);
        assert!(l.check_at(ip("192.0.2.3"), None, later).is_ok());
    }
}
