//! In-memory session store for the browser login (slice 11b; phase 12
//! slice 8 adds a [`Role`] and an optional username per session). Ephemeral
//! by design, matching `gsp-aggregator`'s own posture: a `gsp-ui` restart
//! just logs everyone out — there is nothing here but "is this id currently
//! valid, and what can it do," nothing durable is lost.

use std::collections::HashMap;
use std::sync::RwLock;

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

#[derive(Default)]
pub struct SessionStore {
    sessions: RwLock<HashMap<String, Session>>,
}

impl SessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mints a new session id for `session` and marks it valid.
    pub fn create(&self, session: Session) -> String {
        let mut bytes = [0u8; SESSION_ID_BYTES];
        rand::thread_rng().fill_bytes(&mut bytes);
        let id: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        self.sessions
            .write()
            .expect("session store lock poisoned")
            .insert(id.clone(), session);
        id
    }

    /// The session for `id`, if it's currently valid.
    pub fn get(&self, id: &str) -> Option<Session> {
        self.sessions
            .read()
            .expect("session store lock poisoned")
            .get(id)
            .cloned()
    }

    pub fn is_valid(&self, id: &str) -> bool {
        self.sessions
            .read()
            .expect("session store lock poisoned")
            .contains_key(id)
    }

    pub fn revoke(&self, id: &str) {
        self.sessions
            .write()
            .expect("session store lock poisoned")
            .remove(id);
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
}
