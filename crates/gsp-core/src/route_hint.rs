//! The push-resolver hint table (routing scheme C).
//!
//! A launcher / control plane calls `POST /route-hint { src_ip, pool, ttl_sec }`
//! before a player connects; a listener with `route_hint: true` then checks this
//! table before its route list, and a live `src_ip → pool` hint wins (as long
//! as the pool still exists in the snapshot).
//!
//! Reads are lock-free (an [`arc_swap::ArcSwap`] over a small map); writes are
//! rare (one admin call per session) and prune expired entries as they go.
//! Weakness (see `docs/03`): several clients behind one NAT IP wanting
//! different pools at the same time cannot be told apart — fall back to `dst` /
//! `port` / a token in the first bytes for that case.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;

#[derive(Clone)]
struct Entry {
    pool: String,
    expiry: Instant,
}

#[derive(Default)]
pub struct RouteHints {
    table: ArcSwap<HashMap<IpAddr, Entry>>,
}

impl RouteHints {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Insert / replace the hint for `ip`, and drop any expired entries.
    #[allow(clippy::needless_pass_by_value)] // the table takes ownership of what it stores
    pub fn set(&self, ip: IpAddr, pool: String, ttl: Duration) {
        let expiry = Instant::now() + ttl;
        self.table.rcu(|cur| {
            let now = Instant::now();
            let mut next: HashMap<IpAddr, Entry> = cur
                .iter()
                .filter(|(_, e)| e.expiry > now)
                .map(|(k, e)| (*k, e.clone()))
                .collect();
            next.insert(
                ip,
                Entry {
                    pool: pool.clone(),
                    expiry,
                },
            );
            next
        });
    }

    /// The hinted pool for `ip`, if a non-expired hint exists.
    pub fn lookup(&self, ip: IpAddr) -> Option<String> {
        let t = self.table.load();
        t.get(&ip)
            .filter(|e| e.expiry > Instant::now())
            .map(|e| e.pool.clone())
    }

    /// Number of entries (may include a few not-yet-pruned expired ones).
    pub fn len(&self) -> usize {
        self.table.load().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_lookup_and_expiry() {
        let h = RouteHints::new();
        let ip: IpAddr = "203.0.113.7".parse().unwrap();
        assert_eq!(h.lookup(ip), None);

        h.set(ip, "survival".into(), Duration::from_secs(30));
        assert_eq!(h.lookup(ip).as_deref(), Some("survival"));

        // Replace.
        h.set(ip, "creative".into(), Duration::from_secs(30));
        assert_eq!(h.lookup(ip).as_deref(), Some("creative"));

        // Expired hints do not resolve and are pruned on the next write.
        h.set(ip, "gone".into(), Duration::from_millis(0));
        assert_eq!(h.lookup(ip), None);
        let other: IpAddr = "203.0.113.8".parse().unwrap();
        h.set(other, "lobby".into(), Duration::from_secs(30));
        assert_eq!(h.len(), 1, "the expired entry should have been pruned");
    }
}
