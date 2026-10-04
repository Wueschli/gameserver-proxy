//! In-memory session store for the browser login (slice 11b; phase 12
//! slice 8 adds a [`Role`] and an optional username per session). Ephemeral
//! by design, matching `gsp-aggregator`'s own posture: a `gsp-ui` restart
//! just logs everyone out — there is nothing here but "is this id currently
//! valid, and what can it do," nothing durable is lost.
//!
//! Sessions expire (an idle and an absolute timeout) and the store is
//! capped, so a stolen cookie has a bounded life and `POST /ui/login` can't
//! grow the map without limit.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rand::RngCore;

use crate::role::Role;

/// Byte length of a session id before hex-encoding (32 bytes = 256 bits) —
/// long enough that guessing one is not a realistic attack, which matters
/// more here than for the other bearer tokens in this fleet (those are
/// operator-chosen secrets; this one is generated and only ever needs to
/// resist being *guessed*, not remembered).
const SESSION_ID_BYTES: usize = 32;

#[derive(Debug, Clone)]
pub struct Session {
    pub role: Role,
    /// `None` in legacy `--ui-password` mode (one shared secret, no
    /// identity) — every session there is anonymous `Admin`. `Some` in
    /// `--users-file` mode, used to attribute a proxied write
    /// (`X-Actor`, see `crate::auth`).
    pub username: Option<String>,
}

/// How long a session lives; see [`SessionStore::with_limits`].
#[derive(Debug, Clone, Copy)]
pub struct SessionLimits {
    /// A session unused for this long is gone.
    pub idle_timeout: Duration,
    /// A session older than this is gone however active it is; also the
    /// session cookie's `Max-Age`.
    pub max_age: Duration,
    /// Most live sessions held at once. At the cap the oldest is evicted.
    pub max_sessions: usize,
}

impl Default for SessionLimits {
    fn default() -> Self {
        SessionLimits {
            idle_timeout: Duration::from_secs(30 * 60),
            max_age: Duration::from_secs(12 * 60 * 60),
            max_sessions: 1000,
        }
    }
}

struct Entry {
    session: Session,
    created: Instant,
    last_seen: Instant,
}

pub struct SessionStore {
    limits: SessionLimits,
    sessions: Mutex<HashMap<String, Entry>>,
}

impl Default for SessionStore {
    fn default() -> Self {
        Self::with_limits(SessionLimits::default())
    }
}

impl SessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_limits(limits: SessionLimits) -> Self {
        SessionStore {
            limits,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    pub fn limits(&self) -> SessionLimits {
        self.limits
    }

    fn live(&self, e: &Entry, now: Instant) -> bool {
        now.saturating_duration_since(e.created) < self.limits.max_age
            && now.saturating_duration_since(e.last_seen) < self.limits.idle_timeout
    }

    /// Mints a new session id for `session` and marks it valid.
    pub fn create(&self, session: Session) -> String {
        self.create_at(session, Instant::now())
    }

    fn create_at(&self, session: Session, now: Instant) -> String {
        let mut bytes = [0u8; SESSION_ID_BYTES];
        rand::thread_rng().fill_bytes(&mut bytes);
        let id: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let mut map = self.sessions.lock().expect("session store lock poisoned");
        if map.len() >= self.limits.max_sessions.max(1) {
            map.retain(|_, e| self.live(e, now));
        }
        while map.len() >= self.limits.max_sessions.max(1) {
            let oldest = map
                .iter()
                .min_by_key(|(_, e)| e.created)
                .map(|(k, _)| k.clone())
                .expect("non-empty");
            map.remove(&oldest);
        }
        map.insert(
            id.clone(),
            Entry {
                session,
                created: now,
                last_seen: now,
            },
        );
        id
    }

    /// The session for `id`, if it's currently valid. A hit counts as
    /// activity (resets the idle timeout); an expired one is dropped.
    pub fn get(&self, id: &str) -> Option<Session> {
        self.get_at(id, Instant::now())
    }

    fn get_at(&self, id: &str, now: Instant) -> Option<Session> {
        let mut map = self.sessions.lock().expect("session store lock poisoned");
        let entry = map.get_mut(id)?;
        if !self.live(entry, now) {
            map.remove(id);
            return None;
        }
        entry.last_seen = now;
        Some(entry.session.clone())
    }

    pub fn is_valid(&self, id: &str) -> bool {
        self.get(id).is_some()
    }

    pub fn revoke(&self, id: &str) {
        self.sessions
            .lock()
            .expect("session store lock poisoned")
            .remove(id);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.sessions.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn admin() -> Session {
        Session {
            role: Role::Admin,
            username: None,
        }
    }

    #[test]
    fn a_created_session_is_valid_until_revoked() {
        let store = SessionStore::new();
        let id = store.create(admin());
        assert!(store.is_valid(&id));
        store.revoke(&id);
        assert!(!store.is_valid(&id));
    }

    #[test]
    fn an_unknown_id_is_never_valid() {
        let store = SessionStore::new();
        assert!(!store.is_valid("nonexistent"));
    }

    #[test]
    fn created_ids_are_unique() {
        let store = SessionStore::new();
        let a = store.create(admin());
        let b = store.create(admin());
        assert_ne!(a, b);
    }

    #[test]
    fn revoking_an_unknown_id_is_a_no_op() {
        let store = SessionStore::new();
        store.revoke("nonexistent"); // must not panic
    }

    #[test]
    fn get_returns_the_role_and_username_a_session_was_created_with() {
        let store = SessionStore::new();
        let id = store.create(Session {
            role: Role::Operator,
            username: Some("alice".into()),
        });
        let session = store.get(&id).unwrap();
        assert_eq!(session.role, Role::Operator);
        assert_eq!(session.username.as_deref(), Some("alice"));
    }

    fn limits(idle: u64, max_age: u64, cap: usize) -> SessionLimits {
        SessionLimits {
            idle_timeout: Duration::from_secs(idle),
            max_age: Duration::from_secs(max_age),
            max_sessions: cap,
        }
    }

    #[test]
    fn an_idle_session_expires_but_activity_keeps_it_alive() {
        let store = SessionStore::with_limits(limits(60, 3600, 10));
        let t0 = Instant::now();
        let id = store.create_at(admin(), t0);
        assert!(store.get_at(&id, t0 + Duration::from_secs(50)).is_some());
        // 50s after the last use is still inside the idle window.
        assert!(store.get_at(&id, t0 + Duration::from_secs(100)).is_some());
        // 61s of silence is not.
        assert!(store.get_at(&id, t0 + Duration::from_secs(161)).is_none());
        assert_eq!(store.len(), 0, "an expired session is dropped");
    }

    #[test]
    fn a_session_dies_at_max_age_however_active() {
        let store = SessionStore::with_limits(limits(60, 100, 10));
        let t0 = Instant::now();
        let id = store.create_at(admin(), t0);
        assert!(store.get_at(&id, t0 + Duration::from_secs(50)).is_some());
        assert!(store.get_at(&id, t0 + Duration::from_secs(99)).is_some());
        assert!(store.get_at(&id, t0 + Duration::from_secs(100)).is_none());
    }

    #[test]
    fn the_store_never_exceeds_its_cap_and_evicts_the_oldest() {
        let store = SessionStore::with_limits(limits(60, 3600, 2));
        let t0 = Instant::now();
        let a = store.create_at(admin(), t0);
        let b = store.create_at(admin(), t0 + Duration::from_secs(1));
        let c = store.create_at(admin(), t0 + Duration::from_secs(2));
        assert_eq!(store.len(), 2);
        let now = t0 + Duration::from_secs(3);
        assert!(store.get_at(&a, now).is_none(), "oldest evicted");
        assert!(store.get_at(&b, now).is_some());
        assert!(store.get_at(&c, now).is_some());
    }

    #[test]
    fn a_full_store_drops_expired_sessions_before_evicting_live_ones() {
        let store = SessionStore::with_limits(limits(10, 3600, 2));
        let t0 = Instant::now();
        let stale = store.create_at(admin(), t0);
        let later = t0 + Duration::from_secs(8);
        let live = store.create_at(admin(), later);
        let now = t0 + Duration::from_secs(15);
        let new = store.create_at(admin(), now);
        assert!(store.get_at(&stale, now).is_none());
        assert!(store.get_at(&live, now).is_some(), "live one was kept");
        assert!(store.get_at(&new, now).is_some());
    }
}
