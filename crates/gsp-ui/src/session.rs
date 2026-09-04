//! In-memory session store for the browser login (slice 11b). Ephemeral by
//! design, matching `gsp-aggregator`'s own posture: a `gsp-ui` restart just
//! logs everyone out — there is nothing here but "is this id currently
//! valid," nothing durable is lost.

use std::collections::HashSet;
use std::sync::RwLock;

use rand::RngCore;

/// Byte length of a session id before hex-encoding (32 bytes = 256 bits) —
/// long enough that guessing one is not a realistic attack, which matters
/// more here than for the other bearer tokens in this fleet (those are
/// operator-chosen secrets; this one is generated and only ever needs to
/// resist being *guessed*, not remembered).
const SESSION_ID_BYTES: usize = 32;

#[derive(Default)]
pub struct SessionStore {
    valid: RwLock<HashSet<String>>,
}

impl SessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mints a new session id and marks it valid.
    pub fn create(&self) -> String {
        let mut bytes = [0u8; SESSION_ID_BYTES];
        rand::thread_rng().fill_bytes(&mut bytes);
        let id: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        self.valid
            .write()
            .expect("session store lock poisoned")
            .insert(id.clone());
        id
    }

    pub fn is_valid(&self, id: &str) -> bool {
        self.valid
            .read()
            .expect("session store lock poisoned")
            .contains(id)
    }

    pub fn revoke(&self, id: &str) {
        self.valid
            .write()
            .expect("session store lock poisoned")
            .remove(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_created_session_is_valid_until_revoked() {
        let store = SessionStore::new();
        let id = store.create();
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
        let a = store.create();
        let b = store.create();
        assert_ne!(a, b);
    }

    #[test]
    fn revoking_an_unknown_id_is_a_no_op() {
        let store = SessionStore::new();
        store.revoke("nonexistent"); // must not panic
    }
}
