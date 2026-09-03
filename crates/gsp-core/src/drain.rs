//! Connection tracking for graceful shutdown.
//!
//! Every in-flight proxied connection (a TCP pump task, a UDP session) holds a
//! [`ConnGuard`] handed out by [`ConnTracker::track`]. On shutdown the listener
//! accept loops stop first; [`ConnTracker::wait_idle`] then blocks until the
//! last guard drops, and the caller bounds that wait with a grace period.
//!
//! The counter is a `watch<usize>` so `wait_idle` is a plain
//! [`tokio::sync::watch::Receiver::wait_for`] with no lost-wakeup races. Nothing
//! here is on the per-byte path — a guard is created and dropped once per
//! connection / session.

use std::sync::Arc;

use tokio::sync::watch;

/// Default grace period for [`crate::runtime::Runtime::shutdown`] when the
/// caller does not supply one from config.
pub const DEFAULT_SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

/// Shared counter of live proxied connections / UDP sessions.
#[derive(Debug)]
pub struct ConnTracker {
    tx: watch::Sender<usize>,
}

impl ConnTracker {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            tx: watch::channel(0).0,
        })
    }

    /// Register a connection. The returned guard decrements the count on drop.
    pub fn track(self: &Arc<Self>) -> ConnGuard {
        self.tx.send_modify(|n| *n += 1);
        ConnGuard {
            tracker: self.clone(),
        }
    }

    /// Live connection count.
    pub fn active(&self) -> usize {
        *self.tx.borrow()
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
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.tracker.tx.send_modify(|n| *n = n.saturating_sub(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn wait_idle_returns_when_the_last_guard_drops() {
        let t = ConnTracker::new();
        let g1 = t.track();
        let g2 = t.track();
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
}
