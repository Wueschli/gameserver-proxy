//! Connection tracking for graceful shutdown and `GET /sessions`.
//!
//! Every in-flight proxied connection (a TCP pump task, a UDP session) holds a
//! [`ConnGuard`] handed out by [`ConnTracker::track`]. On shutdown the listener
//! accept loops stop first; [`ConnTracker::wait_idle`] then blocks until the
//! last guard drops, and the caller bounds that wait with a grace period.
//!
//! The counter is a `watch<usize>` so `wait_idle` is a plain
//! [`tokio::sync::watch::Receiver::wait_for`] with no lost-wakeup races. The
//! tracker also keeps a small `id -> metadata` registry (a brief `Mutex` taken
//! once per connection at `track` / drop / `set_target`, never on the per-byte
//! path) that backs the admin API's `GET /sessions`.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::watch;

/// Default grace period for [`crate::runtime::Runtime::shutdown`] when the
/// caller does not supply one from config.
pub const DEFAULT_SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

/// Transport of a tracked connection / session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proto {
    Tcp,
    Udp,
}

impl Proto {
    pub fn as_str(self) -> &'static str {
        match self {
            Proto::Tcp => "tcp",
            Proto::Udp => "udp",
        }
    }
}

/// What the caller knows about a connection / session at `track` time. `pool` /
/// `backend` are not known yet for a TCP connection (routing has not run); they
/// are filled in later via [`ConnGuard::set_target`]. A UDP session knows both
/// at creation and calls `set_target` immediately.
#[derive(Debug, Clone)]
pub struct SessionMeta {
    pub proto: Proto,
    pub listener: String,
    pub peer: SocketAddr,
    pub local: SocketAddr,
}

#[derive(Debug)]
struct Entry {
    proto: Proto,
    listener: String,
    peer: SocketAddr,
    local: SocketAddr,
    pool: Option<String>,
    backend: Option<SocketAddr>,
    since: Instant,
}

/// A point-in-time view of one live connection / session, for `GET /sessions`.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub id: u64,
    pub proto: Proto,
    pub listener: String,
    pub peer: SocketAddr,
    pub local: SocketAddr,
    /// `None` until routing picks a target; `Some("(resolver target)")` for a
    /// pool-less resolver `target`.
    pub pool: Option<String>,
    pub backend: Option<SocketAddr>,
    pub age: Duration,
}

/// Shared counter + registry of live proxied connections / UDP sessions.
#[derive(Debug)]
pub struct ConnTracker {
    tx: watch::Sender<usize>,
    next_id: AtomicU64,
    entries: Mutex<BTreeMap<u64, Entry>>,
}

impl ConnTracker {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            tx: watch::channel(0).0,
            next_id: AtomicU64::new(1),
            entries: Mutex::new(BTreeMap::new()),
        })
    }

    /// Register a connection. The returned guard decrements the count and
    /// removes the registry entry on drop.
    pub fn track(self: &Arc<Self>, meta: SessionMeta) -> ConnGuard {
        self.tx.send_modify(|n| *n += 1);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.entries.lock().unwrap().insert(
            id,
            Entry {
                proto: meta.proto,
                listener: meta.listener,
                peer: meta.peer,
                local: meta.local,
                pool: None,
                backend: None,
                since: Instant::now(),
            },
        );
        ConnGuard {
            tracker: self.clone(),
            id,
        }
    }

    /// Live connection count.
    pub fn active(&self) -> usize {
        *self.tx.borrow()
    }

    /// A snapshot of every live connection / session, oldest id first.
    pub fn sessions(&self) -> Vec<SessionInfo> {
        let now = Instant::now();
        self.entries
            .lock()
            .unwrap()
            .iter()
            .map(|(&id, e)| SessionInfo {
                id,
                proto: e.proto,
                listener: e.listener.clone(),
                peer: e.peer,
                local: e.local,
                pool: e.pool.clone(),
                backend: e.backend,
                age: now.saturating_duration_since(e.since),
            })
            .collect()
    }

    /// Resolve once no tracked connections remain (returns immediately if the
    /// count is already zero). Bound this with a timeout for the grace period.
    pub async fn wait_idle(&self) {
        let mut rx = self.tx.subscribe();
        let _ = rx.wait_for(|n| *n == 0).await;
    }
}

/// Held for the lifetime of one proxied connection / UDP session.
#[derive(Debug)]
pub struct ConnGuard {
    tracker: Arc<ConnTracker>,
    id: u64,
}

impl ConnGuard {
    /// Record the pool (`None` / `Some("(resolver target)")` for a pool-less
    /// target) and backend address routing chose for this connection. Called
    /// once, right after the backend is picked.
    pub fn set_target(&self, pool: Option<&str>, backend: SocketAddr) {
        if let Some(e) = self.tracker.entries.lock().unwrap().get_mut(&self.id) {
            e.pool = pool.map(|s| s.to_string());
            e.backend = Some(backend);
        }
    }
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.tracker.tx.send_modify(|n| *n = n.saturating_sub(1));
        self.tracker.entries.lock().unwrap().remove(&self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn meta() -> SessionMeta {
        SessionMeta {
            proto: Proto::Tcp,
            listener: "l".into(),
            peer: "1.2.3.4:5555".parse().unwrap(),
            local: "0.0.0.0:7777".parse().unwrap(),
        }
    }

    #[tokio::test]
    async fn wait_idle_returns_when_the_last_guard_drops() {
        let t = ConnTracker::new();
        let g1 = t.track(meta());
        let g2 = t.track(meta());
        assert_eq!(t.active(), 2);

        let waiter = {
            let t = t.clone();
            tokio::spawn(async move { t.wait_idle().await })
        };
        drop(g1);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiter.is_finished(), "still one guard held");
        drop(g2);
        tokio::time::timeout(Duration::from_millis(200), waiter)
            .await
            .expect("wait_idle should resolve once idle")
            .unwrap();
    }

    #[tokio::test]
    async fn wait_idle_is_immediate_when_already_idle() {
        let t = ConnTracker::new();
        tokio::time::timeout(Duration::from_millis(50), t.wait_idle())
            .await
            .expect("no guards -> immediate");
    }

    #[tokio::test]
    async fn sessions_reflects_live_entries_and_set_target() {
        let t = ConnTracker::new();
        let g1 = t.track(meta());
        let g2 = t.track(SessionMeta {
            proto: Proto::Udp,
            ..meta()
        });
        g1.set_target(Some("pool-a"), "10.0.0.1:9000".parse().unwrap());

        let mut s = t.sessions();
        assert_eq!(s.len(), 2);
        s.sort_by_key(|e| e.id);
        assert_eq!(s[0].id, 1);
        assert_eq!(s[0].proto, Proto::Tcp);
        assert_eq!(s[0].pool.as_deref(), Some("pool-a"));
        assert_eq!(s[0].backend, Some("10.0.0.1:9000".parse().unwrap()));
        assert_eq!(s[1].proto, Proto::Udp);
        assert!(s[1].pool.is_none());

        drop(g1);
        assert_eq!(t.sessions().len(), 1);
        drop(g2);
        assert!(t.sessions().is_empty());
    }
}
