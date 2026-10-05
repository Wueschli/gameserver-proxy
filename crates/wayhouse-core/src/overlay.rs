//! Runtime backend overlay: admin-API additions and removals layered on top of
//! the file config.
//!
//! The config file stays the source of truth for pool *shape* (balancer, health
//! check, caps). `POST` / `DELETE /pools/{p}/backends[/{addr}]` only edit the
//! *membership* list, and those edits live here — not in the file — so they
//! survive a file reload. Every snapshot rebuild (file reload or an admin edit)
//! runs the file's target list through [`BackendOverlay::effective_targets`].
//!
//! Writes take a short lock and rebuild the map (rare: one admin call);
//! [`effective_targets`] is called once per pool per rebuild, never on the data
//! path.

use std::collections::{BTreeSet, HashMap};
use std::net::SocketAddr;
use std::sync::{Mutex, PoisonError};

/// Per-pool membership edits. `added` and `removed` are disjoint.
#[derive(Debug, Default)]
struct PoolEdits {
    added: BTreeSet<SocketAddr>,
    removed: BTreeSet<SocketAddr>,
}

/// Admin-driven backend add/remove edits, keyed by pool name.
#[derive(Debug, Default)]
pub struct BackendOverlay {
    edits: Mutex<HashMap<String, PoolEdits>>,
}

impl BackendOverlay {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a backend to `pool` (idempotent). Cancels a prior removal.
    pub fn add(&self, pool: &str, addr: SocketAddr) {
        let mut map = self.edits.lock().unwrap_or_else(PoisonError::into_inner);
        let e = map.entry(pool.to_string()).or_default();
        e.removed.remove(&addr);
        e.added.insert(addr);
    }

    /// Remove a backend from `pool` (idempotent). Overrides a file entry and
    /// cancels a prior addition.
    pub fn remove(&self, pool: &str, addr: SocketAddr) {
        let mut map = self.edits.lock().unwrap_or_else(PoisonError::into_inner);
        let e = map.entry(pool.to_string()).or_default();
        e.added.remove(&addr);
        e.removed.insert(addr);
    }

    /// The effective target list for `pool`: the file's `targets` plus overlay
    /// additions, minus overlay removals. Order: file order first (preserved),
    /// then added addresses sorted.
    pub fn effective_targets(&self, pool: &str, file_targets: &[SocketAddr]) -> Vec<SocketAddr> {
        let map = self.edits.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(e) = map.get(pool) else {
            return file_targets.to_vec();
        };
        let mut out: Vec<SocketAddr> = file_targets
            .iter()
            .copied()
            .filter(|a| !e.removed.contains(a))
            .collect();
        for a in &e.added {
            if !out.contains(a) {
                out.push(*a);
            }
        }
        out
    }

    /// `(added, removed)` addresses for `pool` — for `GET /config` introspection.
    pub fn pending(&self, pool: &str) -> (Vec<SocketAddr>, Vec<SocketAddr>) {
        let map = self.edits.lock().unwrap_or_else(PoisonError::into_inner);
        match map.get(pool) {
            Some(e) => (
                e.added.iter().copied().collect(),
                e.removed.iter().copied().collect(),
            ),
            None => (Vec::new(), Vec::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn add_and_remove_layer_over_the_file_list() {
        let o = BackendOverlay::new();
        let file = [a("127.0.0.1:1"), a("127.0.0.1:2")];

        assert_eq!(o.effective_targets("p", &file), file.to_vec());

        o.add("p", a("127.0.0.1:3"));
        assert_eq!(
            o.effective_targets("p", &file),
            vec![a("127.0.0.1:1"), a("127.0.0.1:2"), a("127.0.0.1:3")]
        );

        o.remove("p", a("127.0.0.1:1"));
        assert_eq!(
            o.effective_targets("p", &file),
            vec![a("127.0.0.1:2"), a("127.0.0.1:3")]
        );

        // A different pool is untouched.
        assert_eq!(o.effective_targets("other", &file), file.to_vec());
    }

    #[test]
    fn add_is_idempotent_and_cancels_a_removal() {
        let o = BackendOverlay::new();
        let file = [a("127.0.0.1:1")];
        o.remove("p", a("127.0.0.1:1"));
        assert!(o.effective_targets("p", &file).is_empty());
        o.add("p", a("127.0.0.1:1"));
        assert_eq!(o.effective_targets("p", &file), vec![a("127.0.0.1:1")]);
        o.add("p", a("127.0.0.1:1")); // no duplicate
        assert_eq!(o.effective_targets("p", &file), vec![a("127.0.0.1:1")]);
    }

    #[test]
    fn removing_an_added_backend_leaves_nothing() {
        let o = BackendOverlay::new();
        let file: [SocketAddr; 0] = [];
        o.add("p", a("127.0.0.1:9"));
        o.remove("p", a("127.0.0.1:9"));
        assert!(o.effective_targets("p", &file).is_empty());
        let (added, removed) = o.pending("p");
        assert!(added.is_empty());
        assert_eq!(removed, vec![a("127.0.0.1:9")]);
    }
}
